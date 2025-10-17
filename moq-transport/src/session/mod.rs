mod announce;
mod announced;
mod error;
mod publisher;
mod reader;
mod shared;
mod subscribe;
mod subscribed;
mod subscriber;
mod track_status_requested;
mod writer;

use crate::error::SessionError as OfficialError;
pub use announce::*;
pub use announced::*;
pub use error::*;
pub use publisher::*;
pub use shared::SharedState;
pub use subscribe::*;
pub use subscribed::*;
pub use subscriber::*;
pub use track_status_requested::*;

use reader::*;
use writer::*;

use futures::{stream::FuturesUnordered, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

use crate::watch::Queue;
use crate::util::BandwidthEstimator;
use crate::{message, setup};
use futures::future::BoxFuture;

// A QUIC stat provider trait a transport oldalon
pub trait QuicStatsProvider: Send + Sync {
    fn get_stats(&self) -> BoxFuture<'_, Option<(std::time::Duration, u64, u64, u64, u64)>>;
    fn get_connection(&self) -> quinn::Connection;
}

#[derive(Clone, Copy, Debug)]
pub enum DeadlineMode {
    Edf,
    Lstf,
}

#[derive(Clone, Debug)]
pub struct DeadlineSchedulerConfig {
    pub enabled: bool,
    pub mode: DeadlineMode,
    pub guard_ms: u64,   // γ (biztonsági margó) ms
    pub beta: f64,       // β (pps scaling), pl. 0.9
}

#[must_use = "run() must be called"]
pub struct Session {
    webtransport: web_transport::Session,
    sender: Arc<TokioMutex<Writer>>,
    recver: Reader,
    publisher: Option<Publisher>,
    subscriber: Option<Subscriber>,
    pub outgoing: Queue<message::Message>,

    // Bandwidth estimators for incoming and outgoing data
    pub recv_bandwidth_estimator: Arc<TokioMutex<BandwidthEstimator>>,
    pub send_bandwidth_estimator: Arc<TokioMutex<BandwidthEstimator>>,

    // QUIC stat provider (opcionális)
    quic_stats_provider: Option<Arc<dyn QuicStatsProvider + Send + Sync>>,

    // Send rate limit in bits per second (opcionális)
    // TÖRÖLVE: send_rate_limit_bps: Option<f64>,
    // ÚJ: megosztott limiter (Publisher és Writer-ek részére)
    // pub send_rate_limiter: Option<Arc<TokioMutex<RateLimiter>>>,
    pub deadline_scheduler: Arc<TokioMutex<Option<DeadlineSchedulerConfig>>>,
}

impl Session {
    fn new(
        webtransport: web_transport::Session,
        sender: Writer,
        recver: Reader,
        role: setup::Role,
        _rate_limit_bps: Option<f64>,
        stats: Option<Arc<dyn QuicStatsProvider + Send + Sync>>,
    ) -> (Session, Option<Publisher>, Option<Subscriber>) {
        let (outgoing_send, outgoing_recv) = Queue::default().split();

        // KÖZÖS estimátorok
        let recv_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));
        let send_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));

        // közös deadline handle
        let deadline_scheduler = Arc::new(TokioMutex::new(None));

        // ÚJ: közös rate limit tároló a Publisher számára
        log::debug!("{:?} 3 fasz", _rate_limit_bps);
        let rate_limit_mbps = Arc::new(std::sync::atomic::AtomicU32::new(
                _rate_limit_bps.unwrap_or(0.0) as u32
            ));


        // építs Publisher/Subscriber
        let (publisher, subscriber) = match role {
            setup::Role::Publisher | setup::Role::Both => {
                let publisher = Some(Publisher::with_bandwidth_and_rate_limit(
                    outgoing_send.clone(),
                    webtransport.clone(),
                    send_estimator.clone(),
                    rate_limit_mbps.clone(),
                    deadline_scheduler.clone(),
                    stats.clone(),
                ));
                (publisher, None)
            }
            setup::Role::Subscriber => {
                let subscriber = Some(Subscriber::new(outgoing_send));
                (None, subscriber)
            }
        };
        let session = Session {
            webtransport,
            sender: Arc::new(TokioMutex::new(sender)),
            recver,
            publisher: publisher.clone(),
            subscriber: subscriber.clone(),
            outgoing: outgoing_recv,
            recv_bandwidth_estimator: recv_estimator,
            send_bandwidth_estimator: send_estimator,
            quic_stats_provider: stats.clone(),
            deadline_scheduler,
        };

        (session, publisher, subscriber)
    }

    // Opcionális provider átadása Session-nek
    pub fn with_quic_stats_provider(
        mut self,
        provider: Option<Arc<dyn QuicStatsProvider + Send + Sync>>,

    ) -> Self {
        self.quic_stats_provider = provider;
        self
    }

    // Kényelmi wrapper: connect + provider beállítás
    pub async fn connect_role_with_stats(
        session: web_transport::Session,
        role: setup::Role,
        stats: Option<Arc<dyn QuicStatsProvider + Send + Sync>>,

    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let (session, pubr, subr) = Self::connect_role(session, role).await?;
        Ok((session.with_quic_stats_provider(stats), pubr, subr))
    }

    pub async fn connect(
        session: web_transport::Session,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        Self::connect_role(session, setup::Role::Both).await.map(
            |(session, publisher, subscriber)| (session, publisher.unwrap(), subscriber.unwrap()),
        )
    }

    pub async fn connect_role(
        mut session: web_transport::Session,
        role: setup::Role,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.open_bi().await?;

        let recv_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));
        let send_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));

        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let versions: setup::Versions = [setup::Version::DRAFT_07].into();

        let client = setup::Client {
            role,
            versions: versions.clone(),
            params: Default::default(),
        };

        log::debug!("sending client SETUP: {:?}", client);
        sender.encode(&client).await?;

        let server: setup::Server = recver.decode().await?;
        log::debug!("received server SETUP: {:?}", server);

        let role = match server.role {
            setup::Role::Both => role,
            setup::Role::Publisher => match role {
                setup::Role::Publisher => {
                    return Err(SessionError::RoleIncompatible(server.role, role))
                }
                _ => setup::Role::Subscriber,
            },
            setup::Role::Subscriber => match role {
                setup::Role::Subscriber => {
                    return Err(SessionError::RoleIncompatible(server.role, role))
                }
                _ => setup::Role::Publisher,
            },
        };
        Ok(Session::new(session, sender, recver, role, None, None)) // None rate limit
    }

    // ÚJ: accept_with_stats
    pub async fn accept_with_stats(
        session: web_transport::Session,
        stats: Option<Arc<dyn QuicStatsProvider + Send + Sync>>,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        Self::accept_role_with_stats(session, setup::Role::Both, stats).await
    }

    // ÚJ: accept_role_with_stats
    pub async fn accept_role_with_stats(
        mut session: web_transport::Session,
        role: setup::Role,
        stats: Option<Arc<dyn QuicStatsProvider + Send + Sync>>,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.accept_bi().await?;

        let recv_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));
        let send_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));

        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let client: setup::Client = recver.decode().await?;
        log::debug!("received client SETUP: {:?}", client);

        if !client.versions.contains(&setup::Version::DRAFT_07) {
            return Err(SessionError::Version(client.versions, [setup::Version::DRAFT_07].into()));
        }

        let role = match client.role {
            setup::Role::Both => role,
            setup::Role::Publisher => match role {
                setup::Role::Publisher => return Err(SessionError::RoleIncompatible(client.role, role)),
                _ => setup::Role::Subscriber,
            },
            setup::Role::Subscriber => match role {
                setup::Role::Subscriber => return Err(SessionError::RoleIncompatible(client.role, role)),
                _ => setup::Role::Publisher,
            },
        };

        let server = setup::Server {
            role,
            version: setup::Version::DRAFT_07,
            params: Default::default(),
        };
        log::debug!("sending server SETUP: {:?}", server);
        sender.encode(&server).await?;

        let (session, pubr, subr) = Session::new(session, sender, recver, role, None, stats);
        Ok((session, pubr, subr))
    }

    pub async fn accept(
        session: web_transport::Session,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        Self::accept_role(session, setup::Role::Both).await
    }

    pub async fn accept_role(
        mut session: web_transport::Session,
        role: setup::Role,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.accept_bi().await?;

        let recv_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));
        let send_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));

        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let client: setup::Client = recver.decode().await?;
        log::debug!("received client SETUP: {:?}", client);

        if !client.versions.contains(&setup::Version::DRAFT_07) {
            return Err(SessionError::Version(
                client.versions,
                [setup::Version::DRAFT_07].into(),
            ));
        }

        let role = match client.role {
            setup::Role::Both => role,
            setup::Role::Publisher => match role {
                setup::Role::Publisher => {
                    return Err(SessionError::RoleIncompatible(client.role, role))
                }
                _ => setup::Role::Subscriber,
            },
            setup::Role::Subscriber => match role {
                setup::Role::Subscriber => {
                    return Err(SessionError::RoleIncompatible(client.role, role))
                }
                _ => setup::Role::Publisher,
            },
        };

        let server = setup::Server {
            role,
            version: setup::Version::DRAFT_07,
            params: Default::default(),
        };

        log::debug!("sending server SETUP: {:?}", server);
        sender.encode(&server).await?;
        Ok(Session::new(session, sender, recver, role, None, None)) // None rate limit
    }

    // Hiányzó metódusok hozzáadása
    pub fn publisher(&self) -> Result<&Publisher, SessionError> {
        self.publisher.as_ref().ok_or(SessionError::RoleViolation)
    }

    pub fn subscriber(&self) -> Result<&Subscriber, SessionError> {
        self.subscriber.as_ref().ok_or(SessionError::RoleViolation)
    }

    pub async fn connect_role_with_rate_limit(
        mut session: web_transport::Session,
        role: setup::Role,
        stats: Option<Arc<dyn QuicStatsProvider + Send + Sync>>,
        rate_limit_mbps: Option<f64>,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.open_bi().await?;

        let recv_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));
        let send_estimator = Arc::new(TokioMutex::new(BandwidthEstimator::with_cross_layer()));

        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let versions: setup::Versions = [setup::Version::DRAFT_07].into();

        let client = setup::Client {
            role,
            versions: versions.clone(),
            params: Default::default(),
        };

        log::debug!("sending client SETUP: {:?}", client);
        sender.encode(&client).await?;

        let server: setup::Server = recver.decode().await?;
        log::debug!("received server SETUP: {:?}", server);

        let role = match server.role {
            setup::Role::Both => role,
            setup::Role::Publisher => match role {
                setup::Role::Publisher => {
                    return Err(SessionError::RoleIncompatible(server.role, role))
                }
                _ => setup::Role::Subscriber,
            },
            setup::Role::Subscriber => match role {
                setup::Role::Subscriber => {
                    return Err(SessionError::RoleIncompatible(server.role, role))
                }
                _ => setup::Role::Publisher,
            },
        };

        log::debug!("{:?} 2 fasz", rate_limit_mbps);

        // Biztosítsd, hogy a rate_limit_bps átkerül a Session::new-be
        let (mut session, pubr, subr) = Session::new(session, sender, recver, role, rate_limit_mbps, stats);

        Ok((session, pubr, subr))
    }

    pub fn set_fixed_send_bandwidth_mbps(&mut self, mbps: Option<f64>) {
        if let Some(rate) = mbps {
            log::debug!("(no-op) fixed send bandwidth requested: {:.2} Mbps", rate);
        }
    }

    /// Get the current receive bandwidth estimate in bits per second
    pub async fn recv_bandwidth_bps(&self) -> f64 {
        let estimator = self.recv_bandwidth_estimator.lock().await;
        estimator.bandwidth_bps()
    }

    /// Get the current send bandwidth estimate in bits per second
    pub async fn send_bandwidth_bps(&self) -> f64 {
        let estimator = self.send_bandwidth_estimator.lock().await;
        estimator.bandwidth_bps()
    }

    /// Get the current receive bandwidth estimate in megabits per second
    pub async fn recv_bandwidth_mbps(&self) -> f64 {
        let estimator = self.recv_bandwidth_estimator.lock().await;
        estimator.bandwidth_mbps()
    }

    /// Get the current send bandwidth estimate in megabits per second
    pub async fn send_bandwidth_mbps(&self) -> f64 {
        let estimator = self.send_bandwidth_estimator.lock().await;
        estimator.bandwidth_mbps()
    }

    pub async fn run(self, shared_state: SharedState) -> Result<(), SessionError> {
        let sender = self.sender.clone();
        let shared_state_clone = shared_state.clone();
        let mut this = self;

        let recv_bw_estimator = this.recv_bandwidth_estimator.clone();
        let send_bw_estimator = this.send_bandwidth_estimator.clone();
        let send_bw_estimator_for_watcher = send_bw_estimator.clone();
        let recv_bw_estimator_for_monitor = recv_bw_estimator.clone();
        let send_bw_estimator_for_monitor = send_bw_estimator.clone();
        let deadline_cfg_handle = this.deadline_scheduler.clone();
        let publisher_for_watcher = this.publisher.clone();


        tokio::select! {
            res = Self::run_recv(this.recver, this.publisher, this.subscriber.clone()) => res,
            res = Self::run_send(this.sender, this.outgoing) => res,
            res = Self::run_streams(this.webtransport.clone(), this.subscriber.clone(), this.recv_bandwidth_estimator.clone()) => res,
            res = Self::run_datagrams(this.webtransport, this.subscriber) => res,
            res = Self::bandwidth_message_send_loop(sender.clone(), shared_state_clone.clone()) => Ok(()),
        }
    }

    async fn execute_goaway(
        // FIX: TokioMutex
        sender: Arc<TokioMutex<Writer>>,
        shared_state: SharedState,
    ) -> Result<(), SessionError> {
        let shared_state_clone = shared_state.clone();
        Self::goaway_message_send(sender, shared_state.clone()).await?;
        Self::raise_goaway_timeout_error(shared_state_clone).await
    }

    pub async fn raise_goaway_timeout_error(shared_state: SharedState) -> Result<(), SessionError> {
        tokio::time::sleep(std::time::Duration::from_secs(
            shared_state.get_value().unwrap_or(100000),
        ))
        .await;
        Err(SessionError::GoawayTimeout(OfficialError::GoawayTimeout))
    }

    async fn bandwidth_message_send_loop(
        sender: Arc<TokioMutex<Writer>>,
        shared_state: SharedState,
    ) -> Result<(), SessionError> {
        {
            let bps = shared_state.get_rate_limit_bps().unwrap_or(0);
            let msg = message::Message::FixBandwidth(message::FixBandwidth { bandwidth: bps });
            let mut w = sender.lock().await;
            w.encode(&msg).await?;
            log::debug!("FixBandwidth sent (initial): {} bps ({:.2} Mbps)", bps, (bps as f64)/1_000_000.0);
        }

        loop {
            shared_state.wait_for_rate_limit_change().await;

            let bps = shared_state.get_rate_limit_bps().unwrap_or(0);
            let msg = message::Message::FixBandwidth(message::FixBandwidth { bandwidth: bps });

            let mut w = sender.lock().await;
            w.encode(&msg).await?;
            log::debug!("FixBandwidth sent (update): {} bps ({:.2} Mbps)", bps, (bps as f64)/1_000_000.0);
        }
    }


    async fn run_send(
        // FIX: TokioMutex
        sender: Arc<TokioMutex<Writer>>,
        mut outgoing: Queue<message::Message>,
    ) -> Result<(), SessionError> {
        while let Some(msg) = outgoing.pop().await {
            log::debug!("sending message: {:?}", msg);
            let mut sender = sender.lock().await;
            sender.encode(&msg).await?;
        }
        Ok(())
    }

    async fn goaway_message_send(
        // FIX: ne dőljön el, ha nincs URL beállítva
        sender: Arc<TokioMutex<Writer>>,
        shared_state: SharedState,
    ) -> Result<(), SessionError> {
        // Várj, amíg ténylegesen kapunk GOAWAY URL-t; más változás (pl. rate_limit) esetén csak tovább várunk.
        loop {
            shared_state.wait_for_change().await;

            if let Some(url) = shared_state.get_url() {
                let msg = message::Message::GoAway(message::GoAway {
                    url: url.to_string(),
                });
                let mut sender = sender.lock().await;
                sender.encode(&msg).await?;
                break;
            } else {
                log::debug!("SharedState changed without GOAWAY url; skipping send");
            }
        }
        Ok(())
    }

    pub async fn run_recv(
        mut recver: Reader,
        mut publisher: Option<Publisher>,
        mut subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let msg: message::Message = recver.decode().await?;
            log::debug!("received message: {:?}", msg);

            let msg = match TryInto::<message::Publisher>::try_into(msg) {
                Ok(msg) => {
                    subscriber
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            let msg = match TryInto::<message::Subscriber>::try_into(msg) {
                Ok(msg) => {
                    publisher
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            let msg = match TryInto::<message::Relay>::try_into(msg) {
                Ok(msg) => {
                    if let Some(ref mut pub_) = publisher {
                        if let Err(e) = pub_.recv_goaway(msg.clone()).await {
                            log::warn!("Publisher GoAway Error: {:?}", e);
                        }
                    }
                    if let Some(ref mut sub_) = subscriber {
                        if let Err(e) = sub_.recv_goaway(msg.clone()).await {
                            log::warn!("Subscriber GoAway Error {:?}", e);
                        }
                    }
                    continue;
                }
                Err(msg) => msg,
            };
            unimplemented!("unknown message context: {:?}", msg)
        }
    }

    async fn run_streams(
        mut webtransport: web_transport::Session,
        subscriber: Option<Subscriber>,
        recv_bandwidth_estimator: Arc<TokioMutex<crate::util::BandwidthEstimator>>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                res = webtransport.accept_uni() => {
                    match res {
                        Ok(stream) => {
                            let subscriber = subscriber.clone().ok_or(SessionError::RoleViolation)?;
                            let bandwidth_estimator = recv_bandwidth_estimator.clone();

                            tasks.push(async move {
                                if let Err(err) = Subscriber::recv_stream_with_bandwidth(subscriber, stream, bandwidth_estimator).await {
                                    log::warn!("failed to serve stream: {}", err);
                                };
                            });
                        }
                        Err(e) => {
                            log::debug!("accept_uni ended: {}; treating as graceful stop", e);
                            return Ok(());
                        }
                    }
                },
                _ = tasks.next(), if !tasks.is_empty() => {},
            };
        }
    }

    async fn run_datagrams(
        mut web_transport: web_transport::Session,
        mut subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let datagram = web_transport.recv_datagram().await?;
            subscriber
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_datagram(datagram)?;
        }
    }
}
