use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::coding::Encode;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};
use std::time::{Duration, Instant};


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

    pub async fn serve(mut self, track: serve::TrackReader) -> Result<(), SessionError> {
        let delivery_timeout_ms = self.msg.delivery_timeout_ms;

        if let Some(timeout_ms) = delivery_timeout_ms {
        if let Some(conn) = self.publisher.connection() {
                // Bekapcsoljuk a deadline schedulert a connection-ön
                conn.set_deadline_scheduler(true);

                // Számítsuk ki a deadline-t
                let now = Instant::now();
                let deadline = now + Duration::from_millis(timeout_ms);

                // Becsült objektum méret (pl. 500KB video fragment)
                let estimated_size = 500_000u64;
                conn.set_deadline(estimated_size, Some(deadline), now);
        }
    }

        let res = self.serve_inner(track).await;
        if let Err(err) = &res {
            self.close(err.clone().into())?;
        }

        res
    }

    async fn serve_inner(&mut self, track: serve::TrackReader) -> Result<(), SessionError> {
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
            TrackReaderMode::Subgroups(subgroups) => self.serve_subgroup(subgroups).await,
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
        // Stream módot egyelőre nem támogatunk (a projekt Subgroups módot használ).
        log::warn!("Stream mode is not supported; expected Subgroups mode");
        Err(SessionError::Serve(ServeError::Mode))
    }

    async fn serve_subgroup(
        &mut self,
        mut subgroups: serve::SubgroupsReader,
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

                        tasks.push(async move {
                            if let Err(err) = Self::serve_one_subgroup(header, subgroup, publisher, state).await {
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
) -> Result<(), SessionError> {
    let sg_group_id = header.group_id;
    let sg_subgroup_id = header.subgroup_id;
    let sg_base_prio = subgroup.priority as i32;

    // ✅ 1. Uni stream nyitás
    let mut stream = publisher.open_uni().await?;
    stream.set_priority(sg_base_prio);

    let bandwidth_estimator = publisher.send_bandwidth_estimator.clone();
    let mut writer = Writer::with_bandwidth_estimator(stream, bandwidth_estimator);

    // ✅ 3. SubgroupHeader küldés (ez még deadline ELŐTT megy)
    let header_msg: data::Header = header.clone().into();
    if let Err(e) = writer.encode(&header_msg).await {
        log::warn!(
            "subgroup header write stopped by peer (g={}, sg={}): {e}",
            sg_group_id, sg_subgroup_id
        );
        return Ok(());
    }

    // ✅ 4. Objektumok küldése deadline alatt
    while let Some(mut object) = subgroup.next().await? {
        let ob_hdr = data::SubgroupObject {
            object_id: object.object_id,
            size: object.size,
            status: object.status,
        };

        if let Err(e) = writer.encode(&ob_hdr).await {
            log::warn!(
                "peer stopped at object header (g={}, o={}): {e}",
                subgroup.group_id,
                object.object_id
            );
            while let Some(_chunk) = object.read().await? {}
            let _ = writer.stream.reset(0);
            return Ok(());
        }

        state
            .lock_mut()
            .ok_or(ServeError::Done)?
            .update_max_group_id(subgroup.group_id, object.object_id)?;

        while let Some(chunk) = object.read().await? {
            if let Err(e) = writer.write(&chunk).await {
                log::warn!(
                    "write stopped mid-object (g={}, o={}): {e}",
                    subgroup.group_id,
                    object.object_id
                );
                while let Some(_c) = object.read().await? {}
                let _ = writer.stream.reset(0);
                return Ok(());
            }
        }
    }

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
