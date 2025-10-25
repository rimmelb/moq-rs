use anyhow::{self, Context};
use bytes::{Buf, Bytes};
use moq_transport::serve::{SubgroupWriter, SubgroupsWriter, TrackWriter, TracksWriter};
use mp4::{self, ReadBox, TrackType};
use std::cmp::max;
use std::collections::HashMap;
use std::io::Cursor;
use std::io::Write;
use std::fs;
use std::time;
use std::time::SystemTime;
pub struct Media {
    tracks: HashMap<u32, Track>,
    broadcast: TracksWriter,
    init: SubgroupsWriter,
    catalog: SubgroupsWriter,
    ftyp: Option<Bytes>,
    moov: Option<mp4::MoovBox>,
    current: Option<u32>,
}

impl Media {
    pub fn new(mut broadcast: TracksWriter) -> anyhow::Result<Self> {
        let catalog = broadcast
            .create(".catalog")
            .context("broadcast closed")?
            .groups()?;
        let init = broadcast
            .create("0.mp4")
            .context("broadcast closed")?
            .groups()?;

        Ok(Media {
            tracks: Default::default(),
            broadcast,
            catalog,
            init,
            ftyp: None,
            moov: None,
            current: None,
        })
    }

    pub fn reset(&mut self) {
        for track in self.tracks.values_mut() {
            track.end_group();
        }
    }

    pub fn parse<B: Buf>(&mut self, buf: &mut B) -> anyhow::Result<()> {
        while self.parse_atom(buf)? {}
        Ok(())
    }

    fn parse_atom<B: Buf>(&mut self, buf: &mut B) -> anyhow::Result<bool> {
        let atom = match next_atom(buf)? {
            Some(atom) => atom,
            None => return Ok(false),
        };

        let mut reader = Cursor::new(&atom);
        let header = mp4::BoxHeader::read(&mut reader)?;

        match header.name {
            mp4::BoxType::FtypBox => {
                if self.ftyp.is_some() {
                    tracing::debug!("multiple ftyp atoms");
                    return Ok(true);
                }
                self.ftyp = Some(atom)
            }
            mp4::BoxType::MoovBox => {
                if self.moov.is_some() {
                    tracing::debug!("multiple moov atoms");
                    return Ok(true);
                }

                let moov = mp4::MoovBox::read_box(&mut reader, header.size)?;
                self.setup(&moov, atom)?;
                self.moov = Some(moov);
            }
            mp4::BoxType::MoofBox => {
                let moof = mp4::MoofBox::read_box(&mut reader, header.size)?;
                let fragment = Fragment::new(moof)?;

                if fragment.keyframe {
                    if self
                        .tracks
                        .get(&fragment.track)
                        .context("failed to find track")?
                        .handler
                        == TrackType::Video
                    {
                        for track in self.tracks.values_mut() {
                            track.end_group();
                        }
                    }
                }

                let track = self
                    .tracks
                    .get_mut(&fragment.track)
                    .context("failed to find track")?;

                anyhow::ensure!(self.current.is_none(), "multiple moof atoms");
                self.current.replace(fragment.track);

                track
                    .header(atom, fragment)
                    .context("failed to publish moof")?;
            }
            mp4::BoxType::MdatBox => {
                let track = self.current.take().context("missing moof")?;
                let track = self
                    .tracks
                    .get_mut(&track)
                    .context("failed to find track")?;

                track.data(atom).context("failed to publish mdat")?;
            }
            _ => {}
        }

        Ok(true)
    }

    fn setup(&mut self, moov: &mp4::MoovBox, raw: Bytes) -> anyhow::Result<()> {
        let mut init = self.ftyp.clone().context("missing ftyp")?.to_vec();
        init.extend_from_slice(&raw);

        self.init.append(0)?.write(init.into())?;

        let mut tracks = Vec::new();

        for trak in &moov.traks {
            let id = trak.tkhd.track_id;
            let name = format!("{}.m4s", id);
            let timescale = track_timescale(moov, id);

            {
                let json_path = "tmp/timescales.json";
                let mut map: HashMap<u32, u64> = if let Ok(json) = std::fs::read_to_string(json_path) {
                    serde_json::from_str(&json).unwrap_or_default()
                } else {
                    HashMap::new()
                };
                map.insert(id, timescale);
                std::fs::write(json_path, serde_json::to_string_pretty(&map)?)?;
                log::info!("🕒 Saved timescale for track {id}: {timescale}");
            }

            let handler = (&trak.mdia.hdlr.handler_type).try_into()?;
            let mut selection_params = moq_catalog::SelectionParam::default();

            let mut track = moq_catalog::Track {
                init_track: Some(self.init.name.clone()),
                name: name.clone(),
                namespace: Some(self.broadcast.namespace.to_utf8_path()),
                packaging: Some(moq_catalog::TrackPackaging::Cmaf),
                render_group: Some(1),
                ..Default::default()
            };

            let stsd = &trak.mdia.minf.stbl.stsd;

            if let Some(avc1) = &stsd.avc1 {
                let profile = avc1.avcc.avc_profile_indication;
                let constraints = avc1.avcc.profile_compatibility;
                let level = avc1.avcc.avc_level_indication;
                let width = avc1.width;
                let height = avc1.height;

                let codec = rfc6381_codec::Codec::avc1(profile, constraints, level);
                let codec_str = codec.to_string();

                selection_params.codec = Some(codec_str);
                selection_params.width = Some(width.into());
                selection_params.height = Some(height.into());
            } else if let Some(_hev1) = &stsd.hev1 {
                anyhow::bail!("HEVC not yet supported")
            } else if let Some(mp4a) = &stsd.mp4a {
                let desc = &mp4a
                    .esds
                    .as_ref()
                    .context("missing esds box for MP4a")?
                    .es_desc
                    .dec_config;
                let codec_str = format!(
                    "mp4a.{:02x}.{}",
                    desc.object_type_indication, desc.dec_specific.profile
                );

                selection_params.codec = Some(codec_str);
                selection_params.channel_config = Some(mp4a.channelcount.to_string());
                selection_params.samplerate = Some(mp4a.samplerate.value().into());

                let bitrate = max(desc.max_bitrate, desc.avg_bitrate);
                if bitrate > 0 {
                    selection_params.bitrate = Some(bitrate);
                }
            } else if let Some(vp09) = &stsd.vp09 {
                let vpcc = &vp09.vpcc;
                let codec_str = format!(
                    "vp09.0.{:02x}.{:02x}.{:02x}",
                    vpcc.profile, vpcc.level, vpcc.bit_depth
                );

                selection_params.codec = Some(codec_str);
                selection_params.width = Some(vp09.width.into());
                selection_params.height = Some(vp09.height.into());

                anyhow::bail!("VP9 not yet supported")
            } else {
                anyhow::bail!("unknown codec for track: {}", trak.tkhd.track_id);
            }

            track.selection_params = selection_params;
            tracks.push(track);

            let track = self.broadcast.create(&name).context("broadcast closed")?;
            let track = Track::new(track, handler, timescale);
            self.tracks.insert(id, track);
        }

        let catalog = moq_catalog::Root {
            version: 1,
            streaming_format: 1,
            streaming_format_version: "0.2".to_string(),
            streaming_delta_updates: true,
            common_track_fields: moq_catalog::CommonTrackFields::from_tracks(&mut tracks),
            tracks,
        };

        let catalog_str = serde_json::to_string_pretty(&catalog)?;
        log::info!("catalog: {}", catalog_str);

        self.catalog.append(0)?.write(catalog_str.into())?;

        Ok(())
    }
}

fn next_atom<B: Buf>(buf: &mut B) -> anyhow::Result<Option<Bytes>> {
    let mut peek = Cursor::new(buf.chunk());

    if peek.remaining() < 8 {
        if buf.remaining() != buf.chunk().len() {
            anyhow::bail!("TODO: vectored Buf not yet supported");
        }
        return Ok(None);
    }

    let size = peek.get_u32();
    let _type = peek.get_u32();

    let size = match size {
        0 => anyhow::bail!("TODO: unsupported EOF atom"),
        1 => {
            let size_ext = peek.get_u64();
            anyhow::ensure!(size_ext >= 16, "impossible extended box size: {}", size_ext);
            size_ext as usize
        }
        2..=7 => {
            anyhow::bail!("impossible box size: {}", size)
        }
        size => size as usize,
    };

    if buf.remaining() < size {
        return Ok(None);
    }

    let atom = buf.copy_to_bytes(size);
    Ok(Some(atom))
}

struct Track {
    track: SubgroupsWriter,
    current: Option<SubgroupWriter>,
    timescale: u64,
    handler: TrackType,
    pending: Option<PendingFragment>,
}

struct PendingFragment {
    fragment: Fragment,
    moof_group_id: u64,
    moof_object_id: u64,
}

impl Track {
    fn new(track: TrackWriter, handler: TrackType, timescale: u64) -> Self {
        Self {
            track: track.groups().unwrap(),
            current: None,
            timescale,
            handler,
            pending: None,
        }
    }

    pub fn header(&mut self, raw: Bytes, fragment: Fragment) -> anyhow::Result<()> {
        if let Some(current) = self.current.as_mut() {
            let mut object = current.create(raw.len())?;
            let group_id = object.info.group.group_id;
            let object_id = object.info.object_id;

            fs::create_dir_all("tmp")?;
            let path = format!("tmp/pub_moof_g{}_o{}.bin", group_id, object_id);
            let mut file = std::fs::File::create(&path)?;
            file.write_all(&raw)?;
            //log::debug!("💾 Saved moof fragment to {}", path);

            object.write(raw)?;
            self.pending = Some(PendingFragment {
                fragment: fragment.clone(),
                moof_group_id: group_id,
                moof_object_id: object_id,
            });
            return Ok(());
        }

        let priority: u8 = 127;
        let mut segment = self.track.append(priority)?;
        let group_id = segment.info.group_id;

        let mut object = segment.create(raw.len())?;
        let object_id = object.info.object_id;

        fs::create_dir_all("tmp")?;
        let path = format!("tmp/pub_moof_g{}_o{}.bin", group_id, object_id);
        let mut file = std::fs::File::create(&path)?;
        file.write_all(&raw)?;
        //log::debug!("💾 Saved moof fragment to {}", path);

        object.write(raw)?;
        self.pending = Some(PendingFragment {
            fragment,
            moof_group_id: group_id,
            moof_object_id: object_id,
        });

        self.current = Some(segment);
        Ok(())
    }


    pub fn data(&mut self, raw: Bytes) -> anyhow::Result<()> {
    let pending = match self.pending.take() {
        Some(pending) => pending,
        None => {
            log::warn!("⚠️ No pending fragment when saving mdat");
            return Ok(());
        }
    };

    let segment = self.current.as_mut().context("missing current fragment")?;
    let mut object = segment.create(raw.len())?;

    fs::create_dir_all("tmp")?;
    let track_id = pending.fragment.track;

    // ✅ CRITICAL: Use MOOF group_id and object_id, not MDAT's
    let path = format!(
        "tmp/pub_mdat_g{}_o{}_track{}.bin",
        pending.moof_group_id,
        pending.moof_object_id,
        track_id
    );

    let mut file = std::fs::File::create(&path)?;
    file.write_all(&raw)?;
    //log::debug!("💾 Saved mdat fragment to {}", path);

    object.write(raw)?;

    let start_pts = pending.fragment.timestamp as f64 / self.timescale as f64;
    let duration = 0.042;

    let capture_unix_us = pending.fragment.capture_wallclock
        .duration_since(time::UNIX_EPOCH)
        .unwrap_or_default()  // ✅ Ha hiba van, 0 Duration-t ad vissza
        .as_micros() as u64;


    let manifest_path = format!("tmp/pub_manifest_track{}.txt", track_id);
    let mut manifest_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest_path)?;

    writeln!(
            manifest_file,
            "{}|{}|{}|{:.6}|{:.6}|{}",  // ✅ Added 6th field
            pending.moof_group_id,
            pending.moof_object_id,
            track_id,
            start_pts,
            duration,
            capture_unix_us  // ✅ Publisher wallclock timestamp
        )?;

    Ok(())
    }


    pub fn end_group(&mut self) {
        self.current = None;
    }
}


#[derive(Clone)]
struct Fragment {
    track: u32,
    timestamp: u64,
    keyframe: bool,
    capture_wallclock: SystemTime,
}

impl Fragment {
    fn new(moof: mp4::MoofBox) -> anyhow::Result<Self> {
        anyhow::ensure!(moof.trafs.len() == 1, "multiple tracks per moof atom");
        let track = moof.trafs[0].tfhd.track_id;
        let timestamp = sample_timestamp(&moof).expect("couldn't find timestamp");
        let keyframe = sample_keyframe(&moof);


        Ok(Self {
            track,
            timestamp,
            keyframe,
            capture_wallclock: SystemTime::now()
        })
    }

    fn timestamp(&self, timescale: u64) -> time::Duration {
        time::Duration::from_millis(1000 * self.timestamp / timescale)
    }
}

fn sample_timestamp(moof: &mp4::MoofBox) -> Option<u64> {
    Some(moof.trafs.first()?.tfdt.as_ref()?.base_media_decode_time)
}

fn sample_keyframe(moof: &mp4::MoofBox) -> bool {
    for traf in &moof.trafs {
        let default_flags = traf.tfhd.default_sample_flags.unwrap_or_default();
        let trun = match &traf.trun {
            Some(t) => t,
            None => return false,
        };

        for i in 0..trun.sample_count {
            let mut flags = match trun.sample_flags.get(i as usize) {
                Some(f) => *f,
                None => default_flags,
            };

            if i == 0 && trun.first_sample_flags.is_some() {
                flags = trun.first_sample_flags.unwrap();
            }

            let keyframe = (flags >> 24) & 0x3 == 0x2;
            let non_sync = (flags >> 16) & 0x1 == 0x1;

            if keyframe && !non_sync {
                return true;
            }
        }
    }

    false
}

fn track_timescale(moov: &mp4::MoovBox, track_id: u32) -> u64 {
    let trak = moov
        .traks
        .iter()
        .find(|trak| trak.tkhd.track_id == track_id)
        .expect("failed to find trak");

    trak.mdia.mdhd.timescale as u64
}
