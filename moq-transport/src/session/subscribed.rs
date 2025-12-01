use bytes::BytesMut;
use futures::stream::FuturesUnordered;
use futures::StreamExt;

use std::sync::Arc;

use crate::coding::Encode;
use crate::serve::{ServeError, TrackReaderMode};
use crate::util::MediaQoSReporter;
use crate::watch::State;
use crate::{data, message, serve};
use std::time::{Duration, Instant};
use crate::session::SharedState;


use super::{Publisher, SessionError, SubscribeInfo, Writer};

#[derive(Debug)]
struct SubscribedState {
    max_group_id: Option<(u64, u64)>,
    closed: Result<(), ServeError>,
}

impl SubscribedState {
    fn update_max_group_id(&mut self, group_id: u64, object_id: u64) -> Result<(), ServeError> {
        match self.max_group_id {
            None => {
                self.max_group_id = Some((group_id, object_id));
            }
            Some((max_group, max_object)) => {
                // Lexikografikus frissítés: (g > G) || (g == G && o > O)
                if group_id > max_group || (group_id == max_group && object_id > max_object) {
                    self.max_group_id = Some((group_id, object_id));
                }
            }
        }
        Ok(())
    }
}

impl Default for SubscribedState {
    fn default() -> Self {
        Self {
            max_group_id: None,
            closed: Ok(()),
        }
    }
}

pub struct Subscribed {
    publisher: Publisher,
    state: State<SubscribedState>,
    msg: message::Subscribe,
    ok: bool,

    pub info: SubscribeInfo,
}

impl Subscribed {
    pub(super) fn new(publisher: Publisher, msg: message::Subscribe) -> (Self, SubscribedRecv) {
        let (send, recv) = State::default().split();
        let info = SubscribeInfo {
            namespace: msg.track_namespace.clone(),
            name: msg.track_name.clone(),
        };

        let send = Self {
            publisher,
            state: send,
            msg,
            info,
            ok: false,
        };

        // Prevents updates after being closed
        let recv = SubscribedRecv { state: recv };

        (send, recv)
    }

    pub async fn serve(mut self, track: serve::TrackReader, delivery_timeout: Option<u64>, _shared_state: SharedState, enable_deadline_scheduler: bool, report: Arc<MediaQoSReporter>, enable_relay_side_drop: bool,
    enable_link_capacity_information: bool) -> Result<(), SessionError> {
        let delivery_timeout_ms = self.msg.delivery_timeout_ms;

        let effective_timeout = match (delivery_timeout_ms, delivery_timeout) {
            (Some(ms), _) if ms > 0 => Some(ms),
            (_, Some(t)) => Some(t),
            _ => None,
        };

        if let Some(timeout_ms) = effective_timeout {
            if let Some(conn) = self.publisher.connection() {
                conn.set_deadline_scheduler(enable_deadline_scheduler);
                let now = Instant::now();
                let _deadline = now + Duration::from_millis(timeout_ms);
            }
        }

        let res = self.serve_inner(track, effective_timeout, report, enable_relay_side_drop, enable_link_capacity_information).await;
        res
    }

    async fn serve_inner(&mut self, track: serve::TrackReader, timeout: Option<u64>, report: Arc<MediaQoSReporter>, enable_relay_side_drop: bool,
    enable_link_capacity_information: bool) -> Result<(), SessionError> {
        let latest = track.latest();
        self.state
            .lock_mut()
            .ok_or(ServeError::Cancel)?
            .max_group_id = latest;

        self.publisher.send_message(message::SubscribeOk {
            id: self.msg.id,
            expires: None,
            group_order: message::GroupOrder::Descending, // TODO: resolve correct value from publisher / subscriber prefs
            latest,
        });

        self.ok = true; // So we sent SubscribeDone on drop

        match track.mode().await? {
            // TODO cancel track/datagrams on closed
            TrackReaderMode::Stream(stream) => self.serve_track(stream).await,
            TrackReaderMode::Subgroups(subgroups) => self.serve_subgroup(subgroups, timeout, report, enable_relay_side_drop, enable_link_capacity_information).await,
            TrackReaderMode::Datagrams(datagrams) => self.serve_datagrams(datagrams).await,
        }
    }

    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Done)?;
        state.closed = Err(err);

        Ok(())
    }

    pub async fn closed(&self) -> Result<(), ServeError> {
        loop {
            {
                let state = self.state.lock();
                state.closed.clone()?;

                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(()),
                }
            }
            .await;
        }
    }

    async fn serve_track(&mut self, _track: serve::StreamReader) -> Result<(), SessionError> {
        log::warn!("Stream mode is not supported; expected Subgroups mode");
        Err(SessionError::Serve(ServeError::Mode))
    }

    async fn serve_subgroup(
        &mut self,
        mut subgroups: serve::SubgroupsReader,
        timeout: Option<u64>,
        report: Arc<MediaQoSReporter>,
        enable_relay_side_drop: bool,
        enable_link_capacity_information: bool
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        let mut done: Option<Result<(), ServeError>> = None;
        loop {
            tokio::select! {
                res = subgroups.next(), if done.is_none() => match res {
                    Ok(Some(subgroup)) => {
                        let header = data::SubgroupHeader {
                            subscribe_id: self.msg.id,
                            track_alias: self.msg.track_alias,
                            group_id: subgroup.group_id,
                            subgroup_id: subgroup.subgroup_id,
                            publisher_priority: subgroup.priority,
                        };

                        let publisher = self.publisher.clone();
                        let state = self.state.clone();
                        let info = subgroup.info.clone();

                        let reporter = report.clone();

                        tasks.push(async move {
                            if let Err(err) = Self::serve_one_subgroup(header, subgroup, publisher, state, timeout, reporter, enable_relay_side_drop, enable_link_capacity_information).await {
                                log::warn!("failed to serve group: {:?}, error: {}", info, err);
                            }
                        });
                    },
                    Ok(None) => done = Some(Ok(())),
                    Err(err) => done = Some(Err(err)),
                },
                res = self.closed(), if done.is_none() => done = Some(res),
                _ = tasks.next(), if !tasks.is_empty() => {},
                else => return Ok(done.unwrap()?),
            }
        }
    }


async fn serve_one_subgroup(
    header: data::SubgroupHeader,
    mut subgroup: serve::SubgroupReader,
    mut publisher: Publisher,
    state: State<SubscribedState>,
    timeout: Option<u64>,
    report: Arc<MediaQoSReporter>,
    enable_relay_side_drop: bool,
    enable_link_capacity_information: bool

) -> Result<(), SessionError> {

    let sg_group_id = header.group_id;
    let sg_subgroup_id = header.subgroup_id;
    let sg_base_prio = subgroup.priority as i32;

    if enable_link_capacity_information {
    publisher.set_bandwidth(
        publisher.get_rate_limit_mpbs().map(|r| r as u32)
    );
    }

    let mut stream = publisher.open_uni().await?;
    stream.set_priority(sg_base_prio);

    let mut writer = Writer::new(stream);

    let mut header_size = BytesMut::new();
    let header_msg: data::Header = header.clone().into();
    header_msg.encode(&mut header_size);

    let subgroup_header_len = header_size.len();

    let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;


    if enable_relay_side_drop {
    writer.stream.append_object_size(subgroup_header_len as u64, timeout, Some(time));
    }

    if let Err(e) = writer.encode(&header_msg).await {
        log::debug!(
            "subgroup header write failed (g={}, sg={}): {}. treating as soft drop",
            sg_group_id, sg_subgroup_id, e
        );
        let _ = writer.stream.finish();
        return Ok(());
    }

    while let Some(mut object) = subgroup.next().await? {

        let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

        // Objektum header
        let ob_hdr = data::SubgroupObject {
            object_id: object.object_id,
            size: object.size,
            status: object.status,
            deadline: time
        };
        let ob_header = data::SubgroupObject{
            object_id: object.object_id,
            size: object.size,
            status: object.status,
            deadline: time
        };

        let mut object_header_size = BytesMut::new();
        ob_header.encode(&mut object_header_size);

        //size of the objectheader
        let size_of_object = object.size;
        let size = size_of_object;

        if enable_relay_side_drop {
        writer.stream.append_object_size(size as u64, timeout, Some(time));
        }

        if let Err(e) = writer.encode(&ob_hdr).await {
            log::warn!(
                "peer stopped at object header (g={}, o={}): {e}",
                subgroup.group_id,
                object.object_id
            );
            let _=writer.stream.finish();
            return Ok(());
        }
        state
            .lock_mut()
            .ok_or(ServeError::Done)?
            .update_max_group_id(subgroup.group_id, object.object_id)?;

        // ÚJ: gyűjtsd össze az egész objektumot egyetlen Vec-be
        let mut full_payload = Vec::with_capacity(size_of_object);
        while let Some(chunk) = object.read().await? {
            full_payload.extend_from_slice(&chunk);
        }

        let track_id = format!("{:?}/{}", subgroup.info.namespace.clone(), subgroup.info.track.name.clone());

        // Ellenőrzés: teljes méret megvan-e?
        if full_payload.len() != size_of_object {
            log::warn!("object truncated (g={}, o={}): expected {} B, got {} B. skip", subgroup.group_id, object.object_id, size_of_object, full_payload.len());
            report.record_missing_frames(track_id.clone(), 1);
            report.record_decoder_drop(track_id.clone(), 1);
            return Ok(());
        }
        // Teljes objektum kiírása egyetlen write-tal
        if let Err(e) = writer.write(&full_payload).await {
            log::warn!("write stopped for full object (g={}, o={}): {e}. skip & continue", subgroup.group_id, object.object_id);
            report.record_missing_frames(track_id, 1);
            let _= writer.stream.finish();
            return Ok(());
        }
    }
    let _ = writer.stream.finish();
    Ok(())
}

async fn serve_datagrams(
        &mut self,
        mut datagrams: serve::DatagramsReader,
    ) -> Result<(), SessionError> {
        while let Some(datagram) = datagrams.read().await? {
            let datagram = data::Datagram {
                subscribe_id: self.msg.id,
                track_alias: self.msg.track_alias,
                group_id: datagram.group_id,
                object_id: datagram.object_id,
                publisher_priority: datagram.priority,
                object_status: datagram.status,
                payload_len: datagram.payload.len() as u64,
                payload: datagram.payload,
            };

            let mut buffer = bytes::BytesMut::with_capacity(datagram.payload.len() + 100);
            datagram.encode(&mut buffer)?;

            self.publisher.send_datagram(buffer.into()).await?;
            log::trace!("sent datagram: {:?} bytes", datagram.payload.len());

            self.state
                .lock_mut()
                .ok_or(ServeError::Done)?
                .update_max_group_id(datagram.group_id, datagram.object_id)?;
        }
        Ok(())
    }

}

pub(super) struct SubscribedRecv {
    state: State<SubscribedState>,
}

impl SubscribedRecv {
    pub fn recv_unsubscribe(&mut self) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        if let Some(mut state) = state.into_mut() {
            state.closed = Err(ServeError::Cancel);
        }

        Ok(())
    }
}
