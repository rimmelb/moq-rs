use std::{io::Cursor, sync::Arc, collections::HashMap};
use anyhow::Context;
use log::{debug, info, warn};
use moq_transport::serve::{
    SubgroupObjectReader, SubgroupReader, TrackReaderMode,
    Tracks, TracksReader, TracksWriter,
};
use moq_transport::session::Subscriber;
use moq_transport::util::MediaQoSReporter;
use mp4::{ReadBox, BoxHeader};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    task::JoinSet,
    fs,
};
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Struct definition
// -----------------------------------------------------------------------------
pub struct Media<O> {
    subscriber: Subscriber,
    broadcast: TracksReader,
    tracks_writer: TracksWriter,
    output: Arc<Mutex<O>>,
    init_paths: Arc<Mutex<HashMap<String, String>>>,
    timescales: Arc<HashMap<u32, u32>>,
}

// -----------------------------------------------------------------------------
impl<O: AsyncWrite + Send + Unpin + 'static> Media<O> {
    pub async fn new(
        subscriber: Subscriber,
        tracks: Arc<Tracks>,
        output: O,
    ) -> anyhow::Result<Self> {
        let (tracks_writer, _tracks_request, tracks_reader) = Arc::clone(&tracks).produce();
        let broadcast = tracks_reader;

        let timescales: HashMap<u32, u32> = match fs::read_to_string("tmp/timescales.json").await {
            Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
            Err(_) => {
                warn!("⚠️ Missing tmp/timescales.json, using default 24000");
                HashMap::new()
            }
        };

        Ok(Self {
            subscriber,
            broadcast,
            tracks_writer,
            output: Arc::new(Mutex::new(output)),
            init_paths: Arc::new(Mutex::new(HashMap::new())),
            timescales: Arc::new(timescales),
        })
    }

    pub async fn run(&mut self, reporter: Arc<MediaQoSReporter>) -> anyhow::Result<()> {
        let moov = {
            let init_track_name = "0.mp4";
            let track = self
                .tracks_writer
                .create(init_track_name)
                .context("failed to create init track")?;

            let mut subscriber = self.subscriber.clone();
            tokio::task::spawn(async move {
                subscriber.subscribe(track).await.unwrap_or_else(|err| {
                    warn!("failed to subscribe to init track: {err:?}");
                });
            });

            let track = self
                .broadcast
                .subscribe(init_track_name)
                .context("no init track")?;

            let mut group = match track.mode().await? {
                TrackReaderMode::Subgroups(mut groups) => {
                    groups.next().await?.context("no init group")?
                }
                _ => anyhow::bail!("expected init segment"),
            };

            let object: SubgroupObjectReader = group.next().await?.context("no init fragment")?;
            let buf = Self::recv_object(object).await?;
            fs::create_dir_all("tmp/sub").await.ok();

            let mut reader_clone = Cursor::new(&buf);
            let mut init_data = Vec::new();
            let mut moov_atom = None;

            while let Ok(atom) = read_atom(&mut reader_clone).await {
                init_data.extend_from_slice(&atom);
                if &atom[4..8] == b"moov" {
                    moov_atom = Some(atom.clone());
                }
                if atom.len() < 8 {
                    break;
                }
            }

            info!("✅ full init segment size = {} bytes", init_data.len());
            let moov_bytes = moov_atom.context("no moov atom found")?;

            let mut moov_reader = Cursor::new(&moov_bytes);
            let moov_header = BoxHeader::read(&mut moov_reader)?;
            let moov_box = mp4::MoovBox::read_box(&mut moov_reader, moov_header.size)?;

            for trak in &moov_box.traks {
                let id = trak.tkhd.track_id;
                let init_path = format!("tmp/init_track{}.mp4", id);
                let mut init_file = fs::File::create(&init_path).await?;
                init_file.write_all(&init_data).await?;
                init_file.flush().await?;
                self.init_paths
                    .lock()
                    .await
                    .insert(format!("{}.m4s", id), init_path.clone());
                info!("✅ Saved per-track init segment: {init_path}");
            }

            self.output.lock().await.write_all(&buf).await?;
            moov_box
        };

        let mut has_video = false;
        let mut tracks = vec![];

        for trak in &moov.traks {
            let id = trak.tkhd.track_id;
            let name = format!("{}.m4s", id);
            info!("found track {name}");

            if !has_video && trak.mdia.minf.stbl.stsd.avc1.is_some() {
                has_video = true;
                info!("using {name} for video");

                let track = self
                    .tracks_writer
                    .create(&name)
                    .context("failed to create track")?;

                let mut subscriber = self.subscriber.clone();
                tokio::task::spawn(async move {
                    subscriber.subscribe(track).await.unwrap_or_else(|err| {
                        warn!("failed to subscribe to track: {err:?}");
                    });
                });

                tracks.push(self.broadcast.subscribe(&name).context("no track")?);
            }
        }

        info!("playing {} tracks", tracks.len());
        let mut tasks = JoinSet::new();
        let out = Arc::clone(&self.output);
        let init_paths = Arc::clone(&self.init_paths);
        let timescales = Arc::clone(&self.timescales);

        for track in tracks {
            let out = Arc::clone(&out);
            let reporter_clone = Arc::clone(&reporter);
            let init_paths_clone = Arc::clone(&init_paths);
            let timescales_clone = Arc::clone(&timescales);

            tasks.spawn(async move {
                let name = track.name.clone();
                let report = Arc::clone(&reporter_clone);

                if let Err(err) = async {
                    match track.mode().await? {
                        TrackReaderMode::Subgroups(mut groups) => {
                            while let Some(group) = groups.next().await? {
                                Self::write_subgroup_paired(
                                    group,
                                    Arc::clone(&out),
                                    Arc::clone(&report),
                                    name.clone(),
                                    Arc::clone(&init_paths_clone),
                                    Arc::clone(&timescales_clone),
                                ).await?;
                            }
                        }
                        _ => anyhow::bail!("expected subgroups mode"),
                    }
                    Ok::<_, anyhow::Error>(())
                }.await {
                    warn!("failed to play track {name}: {err:?}");
                }
            });
        }

        while tasks.join_next().await.is_some() {}
        Ok(())
    }

async fn write_subgroup_paired(
    mut group: SubgroupReader,
    out: Arc<Mutex<O>>,
    reporter: Arc<MediaQoSReporter>,
    track_name: String,
    init_paths: Arc<Mutex<HashMap<String, String>>>,
    timescales: Arc<HashMap<u32, u32>>,
) -> anyhow::Result<()> {
    #[derive(Debug)]
    struct Pending {
        group_id: u64,
        object_id: u64,
        bytes: Vec<u8>,
        received_at: Instant,
        media_timestamp: Option<u64>,
    }

    let mut pending: Option<Pending> = None;
    let mut last_render_time: Option<Instant> = None;
    let mut frame_sequence = 0u64;
    let playback_start = Instant::now();

    fs::create_dir_all("tmp/sub").await.ok();

    let track_id: u32 = track_name.split('.').next()
        .unwrap().parse().unwrap_or(1);
    let pub_manifest_path = format!("tmp/pub_manifest_track{}.txt", track_id);

    let mut capture_timestamps: HashMap<u64, Duration> = HashMap::new();
    let mut publisher_start_time: Option<u64> = None;

    if let Ok(manifest_content) = fs::read_to_string(&pub_manifest_path).await {
        for line in manifest_content.lines() {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() >= 6 {
                if let (Ok(object_id), Ok(capture_unix_us)) = (
                    parts[1].parse::<u64>(),
                    parts[5].parse::<u64>(),
                ) {
                    if publisher_start_time.is_none() {
                        publisher_start_time = Some(capture_unix_us);
                    }
                    let relative_duration = Duration::from_micros(
                        capture_unix_us.saturating_sub(publisher_start_time.unwrap())
                    );
                    capture_timestamps.insert(object_id, relative_duration);
                }
            }
        }
        info!("Loaded {} capture timestamps from publisher manifest",
            capture_timestamps.len());
    } else {
        warn!("Publisher manifest not found: {}", pub_manifest_path);
    }

    let subscriber_start = Instant::now();

    while let Some(object) = group.next().await? {
        let g = object.object_id;
        let subgroup_g = object.group_id;
        let declared = object.size;
        let receive_start = Instant::now();

        let mut buf = Vec::with_capacity(declared);
        let mut obj = object;

        while let Some(chunk) = obj.read().await? {
            buf.extend_from_slice(&chunk);
        }

        if buf.len() != declared {
            reporter.record_missing_frames(&track_name, 1);
            continue;
        }
        if buf.len() < 8 { continue; }

        let is_moof = &buf[4..8] == b"moof";
        let is_mdat = &buf[4..8] == b"mdat";

        match (is_moof, is_mdat, pending.is_some()) {
            (true, false, _) => {
                let moof_path = format!("tmp/sub/sub_moof_g{}_o{}.bin", subgroup_g, g);
                fs::write(&moof_path, &buf).await?;

                let media_timestamp = extract_media_timestamp(&buf);

                pending = Some(Pending {
                    group_id: subgroup_g,
                    object_id: g,
                    bytes: buf,
                    received_at: receive_start,
                    media_timestamp,
                });
            }
            (false, true, true) => {
                let moof_pending = pending.take().unwrap();

                let mdat_path = format!(
                    "tmp/sub/sub_mdat_g{}_o{}_track{}.bin",
                    moof_pending.group_id,
                    moof_pending.object_id,
                    track_id
                );
                fs::write(&mdat_path, &buf).await?;

                let mut fused = moof_pending.bytes;
                fused.extend_from_slice(&buf);

                let render_start = Instant::now();
                out.lock().await.write_all(&fused).await?;

                let timescale = timescales.get(&track_id).cloned().unwrap_or(24000);
                let (start_pts, duration_secs) =
                    parse_fragment_timing(&fused, timescale).unwrap_or((0.0, 0.042));

                let capture_ts = capture_timestamps
                    .get(&moof_pending.object_id)
                    .map(|relative_duration| subscriber_start + *relative_duration)
                    .or_else(|| {
                        let estimated_rtt = Duration::from_millis(50);
                        moof_pending.received_at.checked_sub(estimated_rtt / 2)
                    });

                if capture_ts.is_none() {
                    warn!("No capture timestamp for object_id={}",
                        moof_pending.object_id);
                }

                let playback_position = Duration::from_secs_f64(start_pts);
                reporter.record_frame_rendered(
                    &track_name,
                    frame_sequence,
                    capture_ts,
                    render_start,
                    Some(playback_position),
                );
                frame_sequence += 1;

                if let Some(cap_ts) = capture_ts {
                    let latency = render_start.duration_since(cap_ts);
                    if frame_sequence % 100 == 0 {
                        info!("TRUE End-to-end latency: {:.2}ms (frame {})",
                            latency.as_secs_f64() * 1000.0, frame_sequence);
                    }
                }

                if let Some(last_render) = last_render_time {
                    let expected_gap = Duration::from_secs_f64(duration_secs);
                    let actual_gap = render_start.duration_since(last_render);

                    let stall_threshold = Duration::from_millis(100);
                    if actual_gap > expected_gap + stall_threshold {
                        let stall_duration = actual_gap - expected_gap;
                        reporter.record_playback_stall(&track_name, stall_duration);
                        warn!("Playback stall detected: {:.2}ms",
                            stall_duration.as_secs_f64() * 1000.0);
                    }
                }
                last_render_time = Some(render_start);

                if frame_sequence == 1 {
                    let startup_delay = playback_start.elapsed();
                    reporter.record_startup_delay(&track_name, startup_delay);
                }

                let manifest_path = format!("tmp/sub/sub_manifest_track{}.txt", track_id);
                let mut manifest = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&manifest_path)
                    .await?;

                manifest.write_all(format!(
                    "{}|{}|{}|{:.6}|{:.6}\n",
                    moof_pending.group_id,
                    moof_pending.object_id,
                    track_id,
                    start_pts,
                    duration_secs
                ).as_bytes()).await?;

                if frame_sequence % 100 == 0 {
                    reporter.maybe_log_snapshot();
                }
            }
            _ => {}
        }
    }

    reporter.maybe_log_snapshot();

    Ok(())
}



async fn recv_object(mut object: SubgroupObjectReader) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(object.size);
    while let Some(chunk) = object.read().await? {
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
    }
}

// ✅ Helper function to extract tfdt base_media_decode_time
fn extract_media_timestamp(moof_buf: &[u8]) -> Option<u64> {
    use mp4::{BoxHeader, ReadBox, MoofBox};
    use std::io::Cursor;

    let mut reader = Cursor::new(moof_buf);
    let header = BoxHeader::read(&mut reader).ok()?;
    let moof = MoofBox::read_box(&mut reader, header.size).ok()?;
    let traf = moof.trafs.first()?;
    let tfdt = traf.tfdt.as_ref()?;

    Some(tfdt.base_media_decode_time)
}

// -----------------------------------------------------------------------------
async fn read_atom<R: AsyncReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).await?;
    let size = u32::from_be_bytes(buf[0..4].try_into()?) as u64;
    let mut raw = buf.to_vec();
    let mut limit = match size {
        0 => reader.take(u64::MAX),
        1 => {
            reader.read_exact(&mut buf).await?;
            let size_large = u64::from_be_bytes(buf);
            reader.take(size_large - 16)
        }
        2..=7 => anyhow::bail!("impossible box size"),
        size => reader.take(size - 8),
    };
    limit.read_to_end(&mut raw).await?;
    Ok(raw)
}

fn parse_fragment_timing(buf: &[u8], timescale: u32) -> anyhow::Result<(f64, f64)> {
    use anyhow::Context;
    use mp4::{BoxHeader, ReadBox, MoofBox};
    use std::io::Cursor;

    const FLAG_SAMPLE_CTS: u32 = 0x800;

    let mut reader = Cursor::new(buf);
    let header = BoxHeader::read(&mut reader)?;
    let moof = MoofBox::read_box(&mut reader, header.size)?;
    let traf = moof.trafs.first().context("no traf")?;
    let tfdt = traf.tfdt.as_ref().context("no tfdt box")?;
    let base_time = tfdt.base_media_decode_time;

    let mut total_duration = 0u64;
    if let Some(trun) = &traf.trun {
        if !trun.sample_durations.is_empty() {
            for d in &trun.sample_durations {
                total_duration += *d as u64;
            }
        } else if let Some(default) = traf.tfhd.default_sample_duration {
            total_duration = default as u64 * trun.sample_count as u64;
        }
    }

    let mut min_cto = 0i64;
    let mut max_cto = 0i64;

    if let Some(trun) = &traf.trun {
        if (trun.flags & FLAG_SAMPLE_CTS) != 0 && !trun.sample_cts.is_empty() {
            let mut offsets = Vec::with_capacity(trun.sample_cts.len());
            for &cts in &trun.sample_cts {
                let signed = if trun.version == 1 {
                    (cts as i32) as i64
                } else {
                    cts as i64
                };
                offsets.push(signed);
            }

            if !offsets.is_empty() {
                min_cto = *offsets.iter().min().unwrap_or(&0);
                max_cto = *offsets.iter().max().unwrap_or(&0);
            }
        }
    }

    let start_pts = (base_time as i64 + min_cto) as f64 / timescale as f64;
    let end_pts = (base_time as i64 + total_duration as i64 + max_cto) as f64 / timescale as f64;
    let duration = (end_pts - start_pts).max(0.0);

    Ok((start_pts, duration))
}
