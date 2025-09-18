use std::{
    collections::{hash_map, HashMap},
    sync::{Arc, Mutex},
};

use futures::{stream::FuturesUnordered, StreamExt};
use tokio::sync::Mutex as TokioMutex;
use super::writer::RateLimiter;

use crate::{
    coding::Tuple,
    message::{self, Message},
    serve::{ServeError, TracksReader},
    setup,
    util::BandwidthEstimator,
};

use crate::watch::Queue;

use super::{
    Announce, AnnounceRecv, Session, SessionError, Subscribed, SubscribedRecv, TrackStatusRequested,
};

// TODO remove Clone.
#[derive(Clone)]
pub struct Publisher {
    webtransport: web_transport::Session,
    announces: Arc<Mutex<HashMap<Tuple, AnnounceRecv>>>,
    subscribed: Arc<Mutex<HashMap<u64, SubscribedRecv>>>,
    unknown: Queue<Subscribed>,
    outgoing: Queue<Message>,
    url: Arc<Mutex<String>>,

    // Bandwidth estimators for outgoing data streams
    pub send_bandwidth_estimator: Arc<TokioMutex<BandwidthEstimator>>,

    // Rate limit for all outgoing streams
    pub rate_limit_bps: Option<f64>,

    // ÚJ: megosztott limiter
    pub rate_limiter: Option<Arc<TokioMutex<RateLimiter>>>,

    // ÚJ: deadline ütemező konfiguráció
    deadline_scheduler: Arc<TokioMutex<Option<crate::session::DeadlineSchedulerConfig>>>,
}

impl Publisher {
    #[allow(dead_code)] // Keep for backwards compatibility
    pub(crate) fn new(outgoing: Queue<Message>, webtransport: web_transport::Session) -> Self {
        Self {
            webtransport,
            announces: Default::default(),
            subscribed: Default::default(),
            unknown: Default::default(),
            outgoing,
            url: Arc::new(Mutex::new(String::new())),
            send_bandwidth_estimator: Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer())),
            rate_limit_bps: None,
            rate_limiter: None,
            deadline_scheduler: Arc::new(TokioMutex::new(None)),
        }
    }

    pub(crate) fn with_bandwidth_and_rate_limit(
        outgoing: Queue<Message>,
        webtransport: web_transport::Session,
        send_bandwidth_estimator: Arc<TokioMutex<BandwidthEstimator>>,
        rate_limit_bps: Option<f64>,
        rate_limiter: Option<Arc<TokioMutex<RateLimiter>>>,
        deadline_scheduler: Arc<TokioMutex<Option<crate::session::DeadlineSchedulerConfig>>>,
    ) -> Self {
        if let Some(rate) = rate_limit_bps {
            log::info!("Publisher created with rate limit: {:.0} bps ({:.2} Mbps)", rate, rate / 1_000_000.0);
        }
        Self {
            webtransport,
            announces: Default::default(),
            subscribed: Default::default(),
            unknown: Default::default(),
            outgoing,
            url: Arc::new(Mutex::new(String::new())),
            send_bandwidth_estimator,
            rate_limit_bps,
            rate_limiter,
            deadline_scheduler,
        }
    }

    pub fn get_deadline_scheduler(&self) -> Arc<TokioMutex<Option<crate::session::DeadlineSchedulerConfig>>> {
        self.deadline_scheduler.clone()
    }
    pub fn get_rate_limiter(&self) -> Option<Arc<TokioMutex<RateLimiter>>> {
        self.rate_limiter.clone()
    }
    pub fn get_rate_limit_bps(&self) -> Option<f64> {
        self.rate_limit_bps
    }

    pub async fn accept(
        session: web_transport::Session,
    ) -> Result<(Session, Publisher), SessionError> {
        let (session, publisher, _) = Session::accept_role(session, setup::Role::Publisher).await?;
        Ok((session, publisher.unwrap()))
    }

    pub async fn connect(
        session: web_transport::Session,
    ) -> Result<(Session, Publisher), SessionError> {
        let (session, publisher, _) =
            Session::connect_role(session, setup::Role::Publisher).await?;
        Ok((session, publisher.unwrap()))
    }

    pub async fn connect_with_stats_and_rate_limit(
        session: web_transport::Session,
        stats: Option<Arc<dyn super::QuicStatsProvider + Send + Sync>>,
        rate_limit_bps: Option<f64>,

    ) -> Result<(Session, Self), SessionError> {
        let (session, publisher, _) =
            Session::connect_role_with_rate_limit(
                session,
                setup::Role::Publisher,
                stats,
                rate_limit_bps,
            ).await?;
        Ok((session, publisher.unwrap()))
    }

    /// Announce a namespace and serve tracks using the provided [serve::TracksReader].
    /// The caller uses [serve::TracksWriter] for static tracks and [serve::TracksRequest] for dynamic tracks.
    pub async fn announce(&mut self, tracks: TracksReader) -> Result<(), SessionError> {
        let announce = match self
            .announces
            .lock()
            .unwrap()
            .entry(tracks.namespace.clone())
        {
            hash_map::Entry::Occupied(_) => return Err(ServeError::Duplicate.into()),
            hash_map::Entry::Vacant(entry) => {
                let (send, recv) = Announce::new(self.clone(), tracks.namespace.clone());
                entry.insert(recv);
                send
            }
        };

        let mut subscribe_tasks = FuturesUnordered::new();
        let mut status_tasks = FuturesUnordered::new();
        let mut subscribe_done = false;
        let mut status_done = false;

        loop {
            tokio::select! {
                res = announce.subscribed(), if !subscribe_done => {
                    match res? {
                        Some(subscribed) => {
                            let tracks = tracks.clone();

                            subscribe_tasks.push(async move {
                                let info = subscribed.info.clone();
                                if let Err(err) = Self::serve_subscribe(subscribed, tracks).await {
                                    log::warn!("failed serving subscribe: {:?}, error: {}", info, err)
                                }
                            });
                        },
                        None => subscribe_done = true,
                    }

                },
                res = announce.track_status_requested(), if !status_done => {
                    match res? {
                        Some(status) => {
                            let tracks = tracks.clone();

                            status_tasks.push(async move {
                                let info = status.info.clone();
                                if let Err(err) = Self::serve_track_status(status, tracks).await {
                                    log::warn!("failed serving track status request: {:?}, error: {}", info, err)
                                }
                            });
                        },
                        None => status_done = true,
                    }
                },

                Some(res) = subscribe_tasks.next() => res,
                Some(res) = status_tasks.next() => res,
                else => return Ok(())
            }
        }
    }

    pub async fn serve_subscribe(
        subscribe: Subscribed,
        mut tracks: TracksReader,
    ) -> Result<(), SessionError> {
        if let Some(track) = tracks.subscribe(&subscribe.info.name) {
            subscribe.serve(track).await?;
        } else {
            subscribe.close(ServeError::NotFound)?;
        }

        Ok(())
    }

    pub async fn serve_track_status(
        mut track_status_request: TrackStatusRequested,
        mut tracks: TracksReader,
    ) -> Result<(), SessionError> {
        let track = tracks
            .subscribe(&track_status_request.info.track.clone())
            .ok_or(ServeError::NotFound)?;
        let response;

        if let Some((latest_group_id, latest_object_id)) = track.latest() {
            response = message::TrackStatus {
                track_namespace: track_status_request.info.namespace.clone(),
                track_name: track_status_request.info.track.clone(),
                status_code: message::TrackStatusCode::InProgress,
                last_group_id: latest_group_id,
                last_object_id: latest_object_id,
            };
        } else {
            response = message::TrackStatus {
                track_namespace: track_status_request.info.namespace.clone(),
                track_name: track_status_request.info.track.clone(),
                status_code: message::TrackStatusCode::DoesNotExist,
                last_group_id: 0,
                last_object_id: 0,
            };
        }
        // TODO: can we know of any other statuses in this context?

        track_status_request.respond(response).await?;

        Ok(())
    }

    // Returns subscriptions that do not map to an active announce.
    pub async fn subscribed(&mut self) -> Option<Subscribed> {
        self.unknown.pop().await
    }

    pub(crate) fn recv_message(&mut self, msg: message::Subscriber) -> Result<(), SessionError> {
        let res = match msg {
            message::Subscriber::AnnounceOk(msg) => self.recv_announce_ok(msg),
            message::Subscriber::AnnounceError(msg) => self.recv_announce_error(msg),
            message::Subscriber::AnnounceCancel(msg) => self.recv_announce_cancel(msg),
            message::Subscriber::Subscribe(msg) => self.recv_subscribe(msg),
            message::Subscriber::Unsubscribe(msg) => self.recv_unsubscribe(msg),
            message::Subscriber::SubscribeUpdate(msg) => self.recv_subscribe_update(msg),
            message::Subscriber::TrackStatusRequest(msg) => self.recv_track_status_request(msg),
            // TODO: Implement namespace messages.
            message::Subscriber::SubscribeNamespace(_msg) => unimplemented!(),
            message::Subscriber::SubscribeNamespaceOk(_msg) => unimplemented!(),
            message::Subscriber::SubscribeNamespaceError(_msg) => unimplemented!(),
            message::Subscriber::UnsubscribeNamespace(_msg) => unimplemented!(),
            // TODO: Implement fetch messages
            message::Subscriber::Fetch(_msg) => todo!(),
            message::Subscriber::FetchCancel(_msg) => todo!(),
        };

        if let Err(err) = res {
            log::warn!("failed to process message: {}", err);
        }

        Ok(())
    }

    pub async fn recv_goaway(&mut self, msg: message::Relay) -> Result<(), SessionError> {
        log::info!("Megkapja-e ezt?: {:?}", msg);
        let res = match msg {
            message::Relay::GoAway(msg) => self.recv_goaway_message(msg).await,
        };
        if let Err(err) = res {
            log::warn!("failed to process message: {}", err);
        }
        Ok(())
    }

    pub async fn get_url(&self) -> String {
        let url = self.url.lock().unwrap();
        url.clone()
    }

    pub async fn recv_goaway_message(&mut self, msg: message::GoAway) -> Result<(), SessionError> {
        let mut url = self.url.lock().unwrap();
        *url = msg.url.clone();
        if msg.url.is_empty() {
            return Err(SessionError::Serve(ServeError::NotFound));
        }
        Ok(())
    }

    fn recv_announce_ok(&mut self, msg: message::AnnounceOk) -> Result<(), SessionError> {
        if let Some(announce) = self.announces.lock().unwrap().get_mut(&msg.namespace) {
            announce.recv_ok()?;
        }
        Ok(())
    }

    fn recv_announce_error(&mut self, msg: message::AnnounceError) -> Result<(), SessionError> {
        if let Some(announce) = self.announces.lock().unwrap().remove(&msg.namespace) {
            announce.recv_error(ServeError::Closed(msg.error_code))?;
        }

        Ok(())
    }

    fn recv_announce_cancel(&mut self, msg: message::AnnounceCancel) -> Result<(), SessionError> {
        // TODO: If a publisher receives new subscriptions for that namespace after receiving an ANNOUNCE_CANCEL,
        // it SHOULD close the session as a 'Protocol Violation'.
        if let Some(announce) = self.announces.lock().unwrap().remove(&msg.namespace) {
            announce.recv_error(ServeError::Cancel)?;
        }

        Ok(())
    }

    fn recv_subscribe(&mut self, msg: message::Subscribe) -> Result<(), SessionError> {
        let namespace = msg.track_namespace.clone();

        log::info!("{:?} ms", msg.delivery_timeout_ms);
        let subscribe = {
            let mut subscribes = self.subscribed.lock().unwrap();

            // Insert the abort handle into the lookup table.
            let entry = match subscribes.entry(msg.id) {
                hash_map::Entry::Occupied(_) => return Err(SessionError::Duplicate),
                hash_map::Entry::Vacant(entry) => entry,
            };

            let (send, recv) = Subscribed::new(self.clone(), msg);
            entry.insert(recv);

            send
        };

        // If we have an announce, route the subscribe to it.
        if let Some(announce) = self.announces.lock().unwrap().get_mut(&namespace) {
            return announce.recv_subscribe(subscribe).map_err(Into::into);
        }

        // Otherwise, put it in the unknown queue.
        // TODO Have some way to detect if the application is not reading from the unknown queue.
        if let Err(err) = self.unknown.push(subscribe) {
            // Default to closing with a not found error I guess.
            err.close(ServeError::NotFound)?;
        }

        Ok(())
    }

    fn recv_subscribe_update(
        &mut self,
        _msg: message::SubscribeUpdate,
    ) -> Result<(), SessionError> {
        // TODO: Implement updating subscriptions.
        Err(SessionError::Internal)
    }

    fn recv_track_status_request(
        &mut self,
        msg: message::TrackStatusRequest,
    ) -> Result<(), SessionError> {
        let namespace = msg.track_namespace.clone();

        let mut announces = self.announces.lock().unwrap();
        let announce = announces
            .get_mut(&namespace)
            .ok_or(SessionError::Internal)?;

        let track_status_requested = TrackStatusRequested::new(self.clone(), msg);

        announce
            .recv_track_status_requested(track_status_requested)
            .map_err(Into::into)
    }

    fn recv_unsubscribe(&mut self, msg: message::Unsubscribe) -> Result<(), SessionError> {
        if let Some(subscribed) = self.subscribed.lock().unwrap().get_mut(&msg.id) {
            subscribed.recv_unsubscribe()?;
        }
        Ok(())
    }

    pub fn send_message<T: Into<message::Publisher> + Into<Message>>(&mut self, msg: T) {
        let msg = msg.into();
        match &msg {
            message::Publisher::SubscribeDone(msg) => self.drop_subscribe(msg.id),
            message::Publisher::SubscribeError(msg) => self.drop_subscribe(msg.id),
            message::Publisher::Unannounce(msg) => self.drop_announce(&msg.namespace),
            _ => (),
        };
        self.outgoing.push(msg.into()).ok();
    }

    fn drop_subscribe(&mut self, id: u64) {
        self.subscribed.lock().unwrap().remove(&id);
    }

    fn drop_announce(&mut self, namespace: &Tuple) {
        self.announces.lock().unwrap().remove(namespace);
    }

    pub(super) async fn open_uni(&mut self) -> Result<web_transport::SendStream, SessionError> {
        Ok(self.webtransport.open_uni().await?)
    }

    pub(super) async fn send_datagram(&mut self, data: bytes::Bytes) -> Result<(), SessionError> {
        // Rate limit enforcement a datagramokra is
        if let Some(ref limiter) = self.rate_limiter {
            let mut l = limiter.lock().await;
            l.acquire(data.len()).await;
        }

        // Bandwidth accounting
        {
            let mut estimator = self.send_bandwidth_estimator.lock().await;
            estimator.record_bytes(data.len() as u64);
            let _ = estimator.update();
        }

        Ok(self.webtransport.send_datagram(data).await?)
    }
}
