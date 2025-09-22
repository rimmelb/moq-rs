use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::coding::Encode;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};

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

    // Egyszerű becslés: bytes -> ms (send oldali sávszél becslő alapján)
    fn estimate_tx_ms_from_publisher(publisher: &Publisher, bytes: usize) -> Option<f64> {
        if let Ok(e) = publisher.send_bandwidth_estimator.try_lock() {
            let bps = e.bandwidth_bps();
            if bps.is_finite() && bps > 0.0 {
                let bytes_per_sec = bps / 8.0;
                return Some((bytes as f64) * 1000.0 / bytes_per_sec.max(1.0));
            }
        }
        None
    }

    // Slack -> quinn stream priority (0 = highest, 255 = lowest)
    fn priority_from_slack_ms(slack_ms: f64) -> i32 {
        if !slack_ms.is_finite() { return 127; }
        if slack_ms <= 0.0 { return 0; }
        if slack_ms < 50.0 { return 8; }
        if slack_ms < 100.0 { return 16; }
        if slack_ms < 250.0 { return 32; }
        if slack_ms < 500.0 { return 64; }
        if slack_ms < 1000.0 { return 96; }
        127
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
        let delivery_timeout_ms = self.msg.delivery_timeout_ms;

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
                            if let Err(err) = Self::serve_one_subgroup(header, subgroup, publisher, state, delivery_timeout_ms).await {
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

    // Effektív bps (min app_bps, cwnd/RTT), majd pps kiszámítása
    async fn effective_pps(publisher: &Publisher, default_mss: u32, beta: f64) -> Option<f64> {
        let est = publisher.send_bandwidth_estimator.lock().await;
        let mut bps = est.bandwidth_bps(); // app-layer becslés
        if let Some(m) = est.cross_layer_metrics() {
            let rtt_s = m.rtt_current.as_secs_f64();
            if rtt_s > 0.0 && m.cwnd > 0 {
                let cwnd_rate_bps = (m.cwnd as f64 * 8.0) / rtt_s;
                bps = bps.min(cwnd_rate_bps);
            }
            let mss = if m.mss > 0 { m.mss } else { default_mss } as f64;
            let pps = (bps / 8.0) / mss;
            return Some((pps * beta.max(0.1)).max(1.0));
        }
        let mss = default_mss as f64;
        Some(((bps / 8.0) / mss * beta.max(0.1)).max(1.0))
    }

    async fn serve_one_subgroup(
        header: data::SubgroupHeader,
        mut subgroup: serve::SubgroupReader,
        mut publisher: Publisher,
        state: State<SubscribedState>,
        delivery_timeout_ms: Option<u64>,
    ) -> Result<(), SessionError> {
        let mut stream = publisher.open_uni().await?;
        stream.set_priority(subgroup.priority as i32);

        let limiter = publisher.get_rate_limiter();
        let bandwidth_estimator = publisher.send_bandwidth_estimator.clone();
        let mut writer = Writer::with_rate_limit(stream, Some(bandwidth_estimator), limiter);

        let header_msg: data::Header = header.into();
        writer.encode(&header_msg).await?;

        // Ha nincs deadline scheduler, marad az eredeti soros küldés (melegítés/védelem maradhat)
        let cfg_opt = publisher.get_deadline_scheduler().lock().await.clone();
        let Some(cfg) = cfg_opt.filter(|c| c.enabled) else {
            // Ha nincs / túl kicsi sávszél, fallback
            const MIN_START_BPS: f64 = 200_000.0;       // 200 kbps alatt “ismeretlen”
            const DEFAULT_START_BPS: f64 = 5_000_000.0; // 5 Mbps induló becslés

            while let Some(mut object) = subgroup.next().await? {
                let is_init = subgroup.group_id == 0 && object.object_id == 0;

                if let Some(to_ms) = delivery_timeout_ms {
                    // Becsült tx idő – ha nincs / túl kicsi sávszél, fallback
                    let est_tx_ms = if let Some(tx_ms) = Self::estimate_tx_ms_from_publisher(
                        &publisher,
                        (object.size as usize) + 64,
                    ) {
                        tx_ms
                    } else {
                        // nincs becslés → fallback
                        ((object.size as f64 + 64.0) * 8.0 * 1000.0 / DEFAULT_START_BPS)
                    };

                    // Ha a becsült bps túl kicsi volt (→ irreálisan nagy ms), korrigáljuk
                    let adjusted_tx_ms = if est_tx_ms > (to_ms as f64)
                        && !is_init
                    {
                        // Próbáld újraszámolni fallback bps-sel
                        let retry_ms =
                            ((object.size as f64 + 64.0) * 8.0 * 1000.0 / DEFAULT_START_BPS);
                        if retry_ms < est_tx_ms {
                            retry_ms
                        } else {
                            est_tx_ms
                        }
                    } else {
                        est_tx_ms
                    };

                    // Init objektumot SOHA ne dobd
                    if !is_init && adjusted_tx_ms > (to_ms as f64) {
                        log::debug!(
                            "⏱️ drop subgroup object: est_tx={:.1}ms (adj) > timeout={}ms (g={}, o={}, size={}B)",
                            adjusted_tx_ms,
                            to_ms,
                            subgroup.group_id,
                            object.object_id,
                            object.size
                        );
                        while let Some(_chunk) = object.read().await? {}
                        continue;
                    }

                    if !is_init {
                        let slack_ms = (to_ms as f64) - adjusted_tx_ms;
                        let prio = Self::priority_from_slack_ms(slack_ms);
                        writer.stream.set_priority(prio);
                    } else {
                        // Init mindig magas prio
                        writer.stream.set_priority(0);
                    }
                } else {
                    // Nincs timeout: init lehet 0 prio
                    if is_init {
                        writer.stream.set_priority(0);
                    }
                }

                // Küldés
                let hdr = data::SubgroupObject {
                    object_id: object.object_id,
                    size: object.size,
                    status: object.status,
                };
                writer.encode(&hdr).await?;
                state
                    .lock_mut()
                    .ok_or(ServeError::Done)?
                    .update_max_group_id(subgroup.group_id, object.object_id)?;
                while let Some(chunk) = object.read().await? {
                    writer.write(&chunk).await?;
                }
            }
            return Ok(());
        };

        // Egyszerű: minden objektum előtt számold a slack-et és állítsd a stream priority-t.
        let guard = std::time::Duration::from_millis(cfg.guard_ms);
        let now0 = std::time::Instant::now();
        let mss: u32 = 1200;

        while let Some(mut object) = subgroup.next().await? {
            let is_init = subgroup.group_id == 0 && object.object_id == 0;

            // Admission/drop – init soha ne essen ki
            if let Some(to_ms) = delivery_timeout_ms {
                // pps (cross-layer): min(app_bps, cwnd/RTT) → LSTF
                let pps = Self::effective_pps(&publisher, mss, cfg.beta).await.unwrap_or(1.0);
                let rtt = {
                    let est = publisher.send_bandwidth_estimator.lock().await;
                    est.cross_layer_metrics()
                        .map(|m| m.rtt_current)
                        .unwrap_or_else(|| std::time::Duration::from_millis(50))
                };
                let pkt_num = ((object.size as u64 + mss as u64 - 1) / mss as u64).max(1);
                let now = std::time::Instant::now();
                let deadline = now0 + std::time::Duration::from_millis(to_ms);
                let t_finish = now + rtt / 2 + std::time::Duration::from_secs_f64((pkt_num as f64) / pps) + guard;

                if !is_init && t_finish > deadline {
                    log::debug!(
                        "⏱️ drop (admission): cannot meet deadline (g={}, o={}, size={}B)",
                        subgroup.group_id, object.object_id, object.size
                    );
                    while let Some(_chunk) = object.read().await? {}
                    continue;
                }

                // Slack → priority (0 = legmagasabb)
                let slack_ms = (deadline - (now + rtt / 2)).as_secs_f64() - (pkt_num as f64) / pps - guard.as_secs_f64();
                let prio = Self::priority_from_slack_ms(slack_ms);
                writer.stream.set_priority(prio);
            } else if is_init {
                writer.stream.set_priority(0);
            }

            // Küldés (változatlan)
            let hdr = data::SubgroupObject { object_id: object.object_id, size: object.size, status: object.status };
            writer.encode(&hdr).await?;
            state.lock_mut().ok_or(ServeError::Done)?
                .update_max_group_id(subgroup.group_id, object.object_id)?;
            while let Some(chunk) = object.read().await? {
                writer.write(&chunk).await?;
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

            // Transport-level plafon használata esetén itt ne alvassunk (ne legyen app-szintű throttling)

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
