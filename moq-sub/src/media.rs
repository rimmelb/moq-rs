use std::{io::Cursor, sync::Arc};

use anyhow::Context;
use log::{debug, info, trace, warn};
use moq_transport::serve::{
    SubgroupObjectReader, SubgroupReader, TrackReader, TrackReaderMode, Tracks, TracksReader,
    TracksWriter,
};
use moq_transport::session::Subscriber;
use mp4::ReadBox;
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    task::JoinSet,
};

pub struct Media<O> {
    subscriber: Subscriber,
    broadcast: TracksReader,
    tracks_writer: TracksWriter,
    output: Arc<Mutex<O>>,
}

impl<O: AsyncWrite + Send + Unpin + 'static> Media<O> {
    pub async fn new(
        subscriber: Subscriber,
        tracks: Arc<Tracks>,
        output: O,
    ) -> anyhow::Result<Self> {
        let (tracks_writer, _tracks_request, tracks_reader) = Arc::clone(&tracks).produce();
        let broadcast = tracks_reader; // breadcrumb for navigating API name changes
        Ok(Self {
            subscriber,
            broadcast,
            tracks_writer,
            output: Arc::new(Mutex::new(output)),
        })
    }

    // Írj ki egy subgroupot úgy, hogy csak teljes moof+mdat páros menjen ki.
    async fn write_subgroup_paired(mut group: SubgroupReader, out: Arc<Mutex<O>>) -> anyhow::Result<()> {
        #[derive(Debug)]
        struct Pending {
            group_id: u64,
            bytes: Vec<u8>,
        }

        let mut pending: Option<Pending> = None;
        let mut last_group: Option<u64> = None;

        while let Some(object) = group.next().await? {
            let g = object.group_id;
            let declared = object.size;
            let mut buf = Vec::with_capacity(declared);
            let mut read_total = 0usize;
            let mut obj = object;

            while let Some(chunk) = obj.read().await? {
                read_total += chunk.len();
                buf.extend_from_slice(&chunk);
            }

            if read_total != declared {
                log::debug!("drop truncated object g={} declared={} got={}", g, declared, read_total);
                // Truncált → ha moof lett volna, törölj pending-et is
                continue;
            }

            // Gap detektálás (egyszerű heuristic)
            if let Some(prev) = last_group {
                if g > prev + 1 {
                    // gap → resync
                    if pending.is_some() {
                        log::debug!("gap detected ({} -> {}), clearing pending", prev, g);
                        pending = None;
                    }
                }
            }
            last_group = Some(g);

            // Minimum MP4 box header: 8 bájt (size(4)+type(4))
            if buf.len() < 8 {
                log::debug!("object too small for mp4 box g={} size={} -> drop", g, buf.len());
                continue;
            }

            let box_type = &buf[4..8]; // ASCII
            let is_moof = box_type == b"moof";
            let is_mdat = box_type == b"mdat";

            match (is_moof, is_mdat, pending.is_some()) {
                // Új moof, nincs pending → elmentjük
                (true, false, false) => {
                    pending = Some(Pending { group_id: g, bytes: buf });
                }
                // Új moof, de van régi pending moof → régi eldob, új lesz pending
                (true, false, true) => {
                    let old = pending.take().unwrap();
                    log::debug!("resync: moof arrived while pending moof still unmatched (old_g={}), dropping old", old.group_id);
                    pending = Some(Pending { group_id: g, bytes: buf });
                }
                // mdat és van pending moof → párba fűz és kiír
                (false, true, true) => {
                    let moof = pending.take().unwrap();
                    // Sorrend konzisztencia ellenőrzés (nem kötelező)
                    if g < moof.group_id {
                        log::debug!("mdat older than moof (mdat_g={}, moof_g={}), drop mdat", g, moof.group_id);
                        continue;
                    }
                    let mut fused = moof.bytes;
                    fused.extend_from_slice(&buf);
                    {
                        let mut o = out.lock().await;
                        o.write_all(&fused).await?;
                    }
                }
                // mdat pending nélkül → nem tudjuk párosítani
                (false, true, false) => {
                    log::debug!("orphan mdat g={} -> drop", g);
                }
                // Egyéb (más box vagy ismeretlen sorrend) → resync stratégia
                _ => {
                    log::debug!("unknown box type g={} type={:?} pending={} -> drop", g, std::str::from_utf8(box_type).ok(), pending.is_some());
                }
            }
        }

        Ok(())
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
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

            let object = group.next().await?.context("no init fragment")?;
            let buf = Self::recv_object(object).await?;
            self.output.lock().await.write_all(&buf).await?;
            let mut reader = Cursor::new(&buf);

            let ftyp = read_atom(&mut reader).await?;
            anyhow::ensure!(&ftyp[4..8] == b"ftyp", "expected ftyp atom");

            let moov = read_atom(&mut reader).await?;
            anyhow::ensure!(&moov[4..8] == b"moov", "expected moov atom");
            let mut moov_reader = Cursor::new(&moov);
            let moov_header = mp4::BoxHeader::read(&mut moov_reader)?;

            mp4::MoovBox::read_box(&mut moov_reader, moov_header.size)?
        };

        let mut has_video = false;
        let mut has_audio = false; // hagyjuk hamisan, ne írjunk audio-t ugyanarra a kimenetre
        let mut tracks = vec![];
        for trak in &moov.traks {
            let id = trak.tkhd.track_id;
            let name = format!("{}.m4s", id);
            info!("found track {name}");
            let mut active = false;
            if !has_video && trak.mdia.minf.stbl.stsd.avc1.is_some() {
                active = true;
                has_video = true;
                info!("using {name} for video");
            }
            // FONTOS: ne írjunk audio-t ugyanarra a bytestreamre, mert az érvénytelen MP4 lesz.
            // Ha kell audio, írd külön kimenetre és remuxold (lásd lent).
            if active {
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
        for track in tracks {
            let out = self.output.clone();
            tasks.spawn(async move {
                let name = track.name.clone();
                if let Err(err) = async {
                    match track.mode().await? {
                        TrackReaderMode::Subgroups(mut groups) => {
                            while let Some(group) = groups.next().await? {
                                // csak párosan írjuk ki
                                Self::write_subgroup_paired(group, out.clone()).await?;
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

    async fn recv_track(track: TrackReader, out: Arc<Mutex<O>>) -> anyhow::Result<()> {
        let name = track.name.clone();
        debug!("track {name}: start");
        if let TrackReaderMode::Subgroups(mut groups) = track.mode().await? {
            while let Some(group) = groups.next().await? {
                let out = out.clone();
                if let Err(err) = Self::recv_group(group, out).await {
                    warn!("failed to receive group: {err:?}");
                }
            }
        }
        debug!("track {name}: finish");
        Ok(())
    }

    async fn recv_group(mut group: SubgroupReader, out: Arc<Mutex<O>>) -> anyhow::Result<()> {
        while let Some(object) = group.next().await? {
            let expected = object.size;
            let buf = Self::recv_object(object).await?;
            if buf.len() != expected {
                warn!("dropping truncated fragment: expected {}B, got {}B", expected, buf.len());
                continue;
            }
            out.lock().await.write_all(&buf).await?;
        }
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

// Read a full MP4 atom into a vector.
async fn read_atom<R: AsyncReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    // Read the 8 bytes for the size + type
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).await?;

    // Convert the first 4 bytes into the size.
    let size = u32::from_be_bytes(buf[0..4].try_into()?) as u64;

    let mut raw = buf.to_vec();

    let mut limit = match size {
        // Runs until the end of the file.
        0 => reader.take(u64::MAX),

        // The next 8 bytes are the extended size to be used instead.
        1 => {
            reader.read_exact(&mut buf).await?;
            let size_large = u64::from_be_bytes(buf);
            anyhow::ensure!(
                size_large >= 16,
                "impossible extended box size: {}",
                size_large
            );

            reader.take(size_large - 16)
        }

        2..=7 => {
            anyhow::bail!("impossible box size: {}", size)
        }

        size => reader.take(size - 8),
    };

    // Append to the vector and return it.
    let _read_bytes = limit.read_to_end(&mut raw).await?;

    Ok(raw)
}
