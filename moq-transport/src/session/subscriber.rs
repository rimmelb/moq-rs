use std::{
    collections::{hash_map, HashMap},
    io,
    sync::{atomic, Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    coding::{Decode, Tuple},
    data,
    message::{self, Message},
    serve::{self, ServeError},
    setup, util::MediaQoSReporter,
};

use crate::watch::Queue;

use super::{Announced, AnnouncedRecv, Reader, Session, SessionError, Subscribe, SubscribeRecv};

const STALL_THRESHOLD: Duration = Duration::from_millis(750);

// TODO remove Clone.
#[derive(Clone)]
pub struct Subscriber {
    announced: Arc<Mutex<HashMap<Tuple, AnnouncedRecv>>>,
    announced_queue: Queue<Announced>,

    subscribes: Arc<Mutex<HashMap<u64, SubscribeRecv>>>,
    subscribe_next: Arc<atomic::AtomicU64>,
    outgoing: Queue<Message>,
    url: Arc<Mutex<String>>,
}

impl Subscriber {
    pub(super) fn new(outgoing: Queue<Message>) -> Self {
        Self {
            announced: Default::default(),
            announced_queue: Default::default(),
            subscribes: Default::default(),
            subscribe_next: Default::default(),
            outgoing,
            url: Arc::new(Mutex::new(String::new())),
        }
    }

    pub async fn accept(session: web_transport::Session) -> Result<(Session, Self), SessionError> {
        let (session, _, subscriber) =
            Session::accept_role(session, setup::Role::Subscriber).await?;
        Ok((session, subscriber.unwrap()))
    }

    pub async fn connect(session: web_transport::Session) -> Result<(Session, Self), SessionError> {
        let (session, _, subscriber) =
            Session::connect_role(session, setup::Role::Subscriber).await?;
        Ok((session, subscriber.unwrap()))
    }

    pub async fn connect_with_stats(
        session: web_transport::Session,
        stats: Option<Arc<dyn super::QuicStatsProvider + Send + Sync>>,
    ) -> Result<(Session, Self), SessionError> {
        let (session, _, subscriber) =
            Session::connect_role_with_stats(session, setup::Role::Subscriber, stats).await?;
        Ok((session, subscriber.unwrap()))
    }

    pub async fn announced(&mut self) -> Option<Announced> {
        self.announced_queue.pop().await
    }

    pub async fn subscribe(&mut self, track: serve::TrackWriter) -> Result<(), ServeError> {
        let id = self.subscribe_next.fetch_add(1, atomic::Ordering::Relaxed);

        let (send, recv) = Subscribe::new(self.clone(), id, track);
        self.subscribes.lock().unwrap().insert(id, recv);

        send.closed().await
    }

    // Új API: Subscribe deadline-nel
    pub async fn subscribe_with_timeout(
        &mut self,
        track: serve::TrackWriter,
        delivery_timeout_ms: u64,
    ) -> Result<(), ServeError> {
        let id = self.subscribe_next.fetch_add(1, atomic::Ordering::Relaxed);
        let (send, recv) = super::subscribe::Subscribe::new_with_timeout(
            self.clone(),
            id,
            track,
            Some(delivery_timeout_ms),
        );
        self.subscribes.lock().unwrap().insert(id, recv);
        send.closed().await
    }

    pub(super) fn send_message<M: Into<message::Subscriber>>(&mut self, msg: M) {
        let msg = msg.into();

        // Remove our entry on terminal state.
        match &msg {
            message::Subscriber::AnnounceCancel(msg) => self.drop_announce(&msg.namespace),
            message::Subscriber::AnnounceError(msg) => self.drop_announce(&msg.namespace),
            _ => {}
        }
        // TODO report dropped messages?
        let _ = self.outgoing.push(msg.into());
    }

    pub(super) fn recv_message(&mut self, msg: message::Publisher) -> Result<(), SessionError> {
        let res = match &msg {
            message::Publisher::Announce(msg) => self.recv_announce(msg),
            message::Publisher::Unannounce(msg) => self.recv_unannounce(msg),
            message::Publisher::SubscribeOk(msg) => self.recv_subscribe_ok(msg),
            message::Publisher::SubscribeError(msg) => self.recv_subscribe_error(msg),
            message::Publisher::SubscribeDone(msg) => self.recv_subscribe_done(msg),
            message::Publisher::MaxSubscribeId(msg) => self.recv_max_subscribe_id(msg),
            message::Publisher::TrackStatus(msg) => self.recv_track_status(msg),
            // TODO: Implement fetch messages
            message::Publisher::FetchOk(_msg) => todo!(),
            message::Publisher::FetchError(_msg) => todo!(),
        };

        if let Err(SessionError::Serve(err)) = res {
            log::debug!("failed to process message: {:?} {}", msg, err);
            return Ok(());
        }
        res
    }

    //Modification here
    pub async fn recv_goaway(&mut self, msg: message::Relay) -> Result<(), SessionError> {
        let res = match &msg {
            message::Relay::GoAway(msg) => self.recv_goaway_message(msg).await,
            message::Relay::FixBandwidth(_msg) => Ok(()),
        };
        if let Err(SessionError::Serve(err)) = res {
            log::debug!("failed to process message: {:?} {}", msg, err);
            return Ok(());
        }
        Ok(())
    }

    pub async fn recv_goaway_message(&mut self, msg: &message::GoAway) -> Result<(), SessionError> {
        {
            let mut url = self.url.lock().unwrap();
            *url = msg.url.clone();
        }
        Ok(())
    }

    pub fn get_url(&self) -> String {
        let url = self.url.lock().unwrap();
        url.clone()
    }

    fn recv_announce(&mut self, msg: &message::Announce) -> Result<(), SessionError> {
        let mut announces = self.announced.lock().unwrap();

        let entry = match announces.entry(msg.namespace.clone()) {
            hash_map::Entry::Occupied(_) => return Err(SessionError::Duplicate),
            hash_map::Entry::Vacant(entry) => entry,
        };

        let (announced, recv) = Announced::new(self.clone(), msg.namespace.clone());
        if let Err(announced) = self.announced_queue.push(announced) {
            announced.close(ServeError::Cancel)?;
            return Ok(());
        }

        entry.insert(recv);

        Ok(())
    }

    fn recv_unannounce(&mut self, msg: &message::Unannounce) -> Result<(), SessionError> {
        if let Some(announce) = self.announced.lock().unwrap().remove(&msg.namespace) {
            log::info!("received unannounce");
            announce.recv_unannounce()?;
        }

        Ok(())
    }

    fn recv_subscribe_ok(&mut self, msg: &message::SubscribeOk) -> Result<(), SessionError> {
        if let Some(subscribe) = self.subscribes.lock().unwrap().get_mut(&msg.id) {
            subscribe.ok()?;
        }

        Ok(())
    }

    fn recv_subscribe_error(&mut self, msg: &message::SubscribeError) -> Result<(), SessionError> {
        if let Some(subscribe) = self.subscribes.lock().unwrap().remove(&msg.id) {
            subscribe.error(ServeError::Closed(msg.code))?;
        }

        Ok(())
    }

    fn recv_subscribe_done(&mut self, msg: &message::SubscribeDone) -> Result<(), SessionError> {
        if let Some(subscribe) = self.subscribes.lock().unwrap().remove(&msg.id) {
            subscribe.error(ServeError::Closed(msg.code))?;
        }

        Ok(())
    }

    fn recv_max_subscribe_id(
        &mut self,
        _msg: &message::MaxSubscribeId,
    ) -> Result<(), SessionError> {
        // TODO: The Maximum Subscribe Id MUST only increase within a session,
        // and receipt of a MAX_SUBSCRIBE_ID message with an equal or smaller
        // Subscribe ID value is a 'Protocol Violation'
        // The session should be accessible here to check the max_subscribe_id
        Ok(())
    }

    fn recv_track_status(&mut self, _msg: &message::TrackStatus) -> Result<(), SessionError> {
        // TODO: Expose this somehow?
        // TODO: Also add a way to sent a Track Status Request in the first place

        Ok(())
    }

    fn drop_announce(&mut self, namespace: &Tuple) {
        self.announced.lock().unwrap().remove(namespace);
    }

    // #[allow(dead_code)] // Keep for backwards compatibility
    // pub(super) async fn recv_stream(
    //     mut self,
    //     stream: web_transport::RecvStream,
    // ) -> Result<(), SessionError> {
    //     let mut reader = Reader::new(stream);
    //     let header: data::Header = reader.decode().await?;

    //     let id = header.subscribe_id();

    //     let res = self.recv_stream_inner(reader, header).await;
    //     if let Err(SessionError::Serve(err)) = &res {
    //         // The writer is closed, so we should teriminate.
    //         // TODO it would be nice to do this immediately when the Writer is closed.
    //         if let Some(subscribe) = self.subscribes.lock().unwrap().remove(&id) {
    //             subscribe.error(err.clone())?;
    //         }
    //     }

    //     res
    // }

    pub(super) async fn recv_stream_with_bandwidth(
        mut self,
        stream: web_transport::RecvStream,
        report: Arc<MediaQoSReporter>
    ) -> Result<(), SessionError> {
        let mut reader = Reader::new(stream);
        let header: data::Header = match reader.decode().await {
            Ok(header) => header,
            Err(SessionError::Decode(crate::coding::DecodeError::More(_))) => {
                log::debug!(
                    "data stream ended before header could be decoded; treating as soft drop"
                );
                report.record_missing_frames("unknown".to_string(), 1);
                report.record_decoder_drop("unknown".to_string(), 1);
                return Ok(());
            }
            Err(SessionError::Decode(crate::coding::DecodeError::Io(err))) => {
                log::debug!(
                    "data stream I/O error before header could be decoded: {}; treating as drop",
                    err
                );
                report.record_missing_frames("unknown".to_string(), 1);
                report.record_decoder_drop("unknown".to_string(), 1);
                return Ok(());
            }
            Err(SessionError::Transport(err)) => {
                log::debug!(
                    "data stream reset before header could be decoded: {}; ignoring",
                    err
                );
                report.record_missing_frames("unknown".to_string(), 1);
                report.record_decoder_drop("unknown".to_string(), 1);
                return Ok(());
            }
            Err(err) => return Err(err),

        };
        let id = header.subscribe_id();

        let res = self.recv_stream_inner(reader, header, report.clone()).await;

        match &res {
            Err(SessionError::Serve(ServeError::Cancel)) => {
                report.record_missing_frames("unknown".to_string(), 1);
                report.record_decoder_drop("unknown".to_string(), 1);
                return Ok(());
            }
            Err(SessionError::Transport(e)) => {
                report.record_missing_frames("unknown".to_string(), 1);
                report.record_decoder_drop("unknown".to_string(), 1);
                log::debug!("data stream for subscribe id={} reset by peer: {}; ignoring", id, e);
                return Ok(());
            }
            Err(SessionError::Serve(err)) => {
                report.record_missing_frames("unknown".to_string(), 1);
                report.record_decoder_drop("unknown".to_string(), 1);
                if let Some(subscribe) = self.subscribes.lock().unwrap().remove(&id) {
                    subscribe.error(err.clone())?;
                }
            }
            _ => {}
        }

        res
    }

    async fn recv_stream_inner(
        &mut self,
        reader: Reader,
        header: data::Header,
        report: Arc<MediaQoSReporter>
    ) -> Result<(), SessionError> {
        let id = header.subscribe_id();

        // This is super silly, but I couldn't figure out a way to avoid the mutex guard across awaits.
        enum Writer {
            Track(serve::StreamWriter),
            Subgroup(serve::SubgroupWriter),
        }

        let writer = {
            let mut subscribes = self.subscribes.lock().unwrap();
            let subscribe = subscribes.get_mut(&id).ok_or(ServeError::NotFound)?;

            match header {
                data::Header::Track(track) => Writer::Track(subscribe.track(track)?),
                data::Header::Subgroup(subgroup) => Writer::Subgroup(subscribe.subgroup(subgroup)?),
            }
        };

        match writer {
            Writer::Track(track) => Self::recv_track(track, reader).await?,
            Writer::Subgroup(group) => Self::recv_subgroup(group, reader, report).await?,
        };

        Ok(())
    }

    async fn recv_track(
        mut track: serve::StreamWriter,
        mut reader: Reader,
    ) -> Result<(), SessionError> {
        log::trace!("received track: {:?}", track.info);

        let mut prev: Option<serve::StreamGroupWriter> = None;

        while !reader.done().await? {
            let chunk: data::TrackObject = reader.decode().await?;

            let mut group = match prev {
                Some(group) if group.group_id == chunk.group_id => group,
                _ => track.create(chunk.group_id)?,
            };

            let mut object = group.create(chunk.size)?;

            let mut remain = chunk.size;
            while remain > 0 {
                match reader.read_chunk(remain).await? {
                    Some(bytes) => {
                        remain -= bytes.len();
                        object.write(bytes)?;
                    }
                    None => {
                        log::debug!("recv_track: truncated object (g={}, remain={}B), dropping object and continuing",
                            chunk.group_id, remain);
                        break;
                    }
                }
            }
            prev = Some(group);
        }

        Ok(())

    }

    async fn recv_subgroup(
        mut group: serve::SubgroupWriter,
        mut reader: Reader,
        report: Arc<MediaQoSReporter>,
    ) -> Result<(), SessionError> {
        log::trace!("received subgroup: {:?}", group.info);

        let track_id = group.info.track.name.clone();

        while !reader.done().await? {
            let hdr: data::SubgroupObject = match reader.decode().await {
                Ok(h) => h,
                Err(e) => {
                    log::debug!("recv_subgroup: failed to decode object header: {e}; ending subgroup");
                    return Ok(());
                }
            };

            let mut object = group.create(hdr.size)?;
            let mut remain = hdr.size;
            let mut write_failed = false;

            while remain > 0 {
                match reader.read_chunk(remain).await {
                    Ok(Some(bytes)) => {
                        remain -= bytes.len();
                        if let Err(e) = object.write(bytes) {
                            log::warn!("recv_subgroup: write failed (remain={}B): {e}; draining rest", remain);
                            write_failed = true;
                            break;
                        }
                    }
                    Ok(None) => {
                        log::debug!("recv_subgroup: truncated object (remain={}B), draining", remain);
                        write_failed = true;
                        break;
                    }
                    Err(err) => {
                        log::debug!("recv_subgroup: transport error: {err}; draining rest");
                        write_failed = true;
                        break;
                    }
                }
            }

            if write_failed {
                while remain > 0 {
                    match reader.read_chunk(remain).await {
                        Ok(Some(bytes)) => remain -= bytes.len(),
                        Ok(None) => {
                            log::debug!("recv_subgroup: stream ended while draining failed object");
                            return Ok(());
                        }
                        Err(e) => {
                            log::debug!("recv_subgroup: error while draining failed object: {e}");
                            return Ok(());
                        }
                    }
                }
                report.record_missing_frames(track_id.clone(), 1);
                report.record_decoder_drop(track_id.clone(), 1);
                continue;
            }
        }
        Ok(())
    }

    pub fn recv_datagram(&mut self, datagram: bytes::Bytes) -> Result<(), SessionError> {
        let mut cursor = io::Cursor::new(datagram);
        let datagram = data::Datagram::decode(&mut cursor)?;

        if let Some(subscribe) = self
            .subscribes
            .lock()
            .unwrap()
            .get_mut(&datagram.subscribe_id)
        {
            subscribe.datagram(datagram)?;
        }

        Ok(())
    }
}
