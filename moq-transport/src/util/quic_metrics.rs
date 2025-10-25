use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Thread-safe reporter for media quality-of-service statistics.
///
/// The reporter keeps per-track aggregates so higher layers can log or export
/// metrics such as missing frames, playback stalls, startup delay, latency, and
/// objective quality scores (VMAF / PSNR / SSIM).
#[derive(Clone, Default)]
pub struct MediaQoSReporter {
    inner: Arc<Mutex<MediaQoSInner>>,
}

impl MediaQoSReporter {
    /// Create a new reporter without periodic logging.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a reporter that throttles [`maybe_log_snapshot`] calls to the provided interval.
    pub fn with_report_interval(interval: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MediaQoSInner::with_interval(Some(interval)))),
        }
    }

    /// Update (or disable) the automatic reporting interval.
    pub fn set_report_interval(&self, interval: Option<Duration>) {
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        inner.report_interval = interval;
        inner.last_report = Instant::now();
    }

    /// Record that the decoder explicitly dropped frames.
    pub fn record_decoder_drop(&self, track: impl Into<String>, dropped: u64) {
        if dropped == 0 {
            return;
        }
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        let stats = inner.track_mut(track);
        stats.decoder_dropped = stats.decoder_dropped.saturating_add(dropped);
    }

    /// Record a rendered frame.
    ///
    /// * `sequence` must be monotonically increasing per track (object id or frame index).
    /// * `capture_ts` is the time when the frame was produced; latency uses `render_ts - capture_ts`.
    /// * `playback_position` is the intended presentation timestamp relative to playback start.
    pub fn record_frame_rendered(
        &self,
        track: impl Into<String>,
        sequence: u64,
        capture_ts: Option<Instant>,
        render_ts: Instant,
        playback_position: Option<Duration>,
    ) {
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        let stats = inner.track_mut(track);

        if let Some(last) = stats.last_sequence {
            if sequence > last + 1 {
                stats.missing_frames = stats
                    .missing_frames
                    .saturating_add(sequence - last - 1);
            }
        }

        stats.total_frames = stats.total_frames.saturating_add(1);
        stats.last_sequence = Some(sequence);

        if let Some(capture) = capture_ts {
            if let Some(latency) = render_ts.checked_duration_since(capture) {
                stats.frame_latency.add_duration(latency);
            }
        }

        if let Some(prev_render) = stats.last_render {
            if let Some(render_gap) = render_ts.checked_duration_since(prev_render) {
                if let (Some(curr_playback), Some(prev_playback)) =
                    (playback_position, stats.last_playback_position)
                {
                    if let Some(playback_gap) = curr_playback.checked_sub(prev_playback) {
                        let jitter =
                            (render_gap.as_secs_f64() - playback_gap.as_secs_f64()).abs();
                        if jitter.is_finite() {
                            stats.jitter.add_seconds(jitter);
                        }
                    }
                }
            }
        }

        stats.last_render = Some(render_ts);
        if let Some(pos) = playback_position {
            stats.last_playback_position = Some(pos);
        }
    }

    /// Register a playback stall (buffer underrun).
    pub fn record_playback_stall(&self, track: impl Into<String>, duration: Duration) {
        if duration.is_zero() {
            return;
        }
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        let stats = inner.track_mut(track);
        stats.stall_events = stats.stall_events.saturating_add(1);
        stats.total_stall_time += duration;
        stats.stall_durations.add_duration(duration);
    }

    /// Record startup delay from playback start until first frame shown.
    pub fn record_startup_delay(&self, track: impl Into<String>, delay: Duration) {
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        let stats = inner.track_mut(track);
        if stats.startup_delay.is_none() {
            stats.startup_delay = Some(delay);
        }
        stats.startup_delay_stats.add_duration(delay);
    }

    /// Record a VMAF score (0-100).
    pub fn record_vmaf(&self, track: impl Into<String>, score: f64) {
        self.record_score(track, score, |stats| &mut stats.vmaf);
    }

    /// Record a PSNR value in dB.
    pub fn record_psnr(&self, track: impl Into<String>, score: f64) {
        self.record_score(track, score, |stats| &mut stats.psnr);
    }

    /// Record an SSIM value (0-1).
    pub fn record_ssim(&self, track: impl Into<String>, score: f64) {
        self.record_score(track, score, |stats| &mut stats.ssim);
    }

    /// Record an arbitrary number of missing frames detected out-of-band.
    pub fn record_missing_frames(&self, track: impl Into<String>, count: u64) {
        if count == 0 {
            return;
        }
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        let stats = inner.track_mut(track);
        stats.missing_frames = stats.missing_frames.saturating_add(count);
    }

    /// Snapshot the current statistics for every track.
    pub fn snapshot(&self) -> Vec<TrackQoSReport> {
        let inner = self.inner.lock().expect("qos reporter poisoned");
        inner
            .tracks
            .iter()
            .map(|(track, stats)| stats.to_report(track))
            .collect()
    }

    /// Log a condensed snapshot if the reporting interval has elapsed.
    pub fn maybe_log_snapshot(&self) {
        let reports = {
            let mut inner = self.inner.lock().expect("qos reporter poisoned");
            match inner.report_interval {
                Some(interval) => {
                    if inner.last_report.elapsed() < interval {
                        return;
                    }
                    inner.last_report = Instant::now();
                    inner
                        .tracks
                        .iter()
                        .map(|(track, stats)| stats.to_report(track))
                        .collect::<Vec<_>>()
                }
                None => inner
                    .tracks
                    .iter()
                    .map(|(track, stats)| stats.to_report(track))
                    .collect(),
            }
        };

        for report in reports {
            let mut line = format!(
                "qos track={} frames={} missing={} decoder_dropped={} stall_time_ms={:.2} stall_events={}",
                report.track,
                report.total_frames,
                report.missing_frames,
                report.decoder_dropped,
                report.total_stall_time.as_secs_f64() * 1_000.0,
                report.stall_events
            );

            if let Some(latency) = &report.frame_latency {
                line.push_str(&format!(
                    " latency_avg_ms={:.2} latency_p95_ms={:.2}",
                    latency.mean.as_secs_f64() * 1_000.0,
                    latency.max.as_secs_f64() * 1_000.0
                ));
            }

            if let Some(jitter) = &report.playback_jitter {
                line.push_str(&format!(
                    " jitter_avg_ms={:.3}",
                    jitter.mean.as_secs_f64() * 1_000.0
                ));
            }

            if let Some(vmaf) = &report.vmaf {
                line.push_str(&format!(" vmaf_mean={:.2}", vmaf.mean));
            }

            if let Some(psnr) = &report.psnr {
                line.push_str(&format!(" psnr_mean={:.2}", psnr.mean));
            }

            if let Some(ssim) = &report.ssim {
                line.push_str(&format!(" ssim_mean={:.4}", ssim.mean));
            }

            if let Some(startup) = report.startup_delay {
                line.push_str(&format!(
                    " startup_ms={:.2}",
                    startup.as_secs_f64() * 1_000.0
                ));
            }

            log::info!("{}", line);
        }
    }

    /// Reset all metrics for a track.
    pub fn clear_track(&self, track: &str) {
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        inner.tracks.remove(track);
    }

    /// Reset metrics for every track.
    pub fn clear_all(&self) {
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        inner.tracks.clear();
    }

    fn record_score<F>(&self, track: impl Into<String>, score: f64, select: F)
    where
        F: Fn(&mut TrackStats) -> &mut RunningStats,
    {
        if !score.is_finite() {
            return;
        }
        let mut inner = self.inner.lock().expect("qos reporter poisoned");
        let stats = inner.track_mut(track);
        select(stats).add(score);
    }
}

struct MediaQoSInner {
    tracks: HashMap<String, TrackStats>,
    report_interval: Option<Duration>,
    last_report: Instant,
}

impl Default for MediaQoSInner {
    fn default() -> Self {
        Self {
            tracks: HashMap::new(),
            report_interval: None,
            last_report: Instant::now(),
        }
    }
}

impl MediaQoSInner {
    fn with_interval(interval: Option<Duration>) -> Self {
        Self {
            tracks: HashMap::new(),
            report_interval: interval,
            last_report: Instant::now(),
        }
    }

    fn track_mut(&mut self, track: impl Into<String>) -> &mut TrackStats {
        self.tracks
            .entry(track.into())
            .or_insert_with(TrackStats::default)
    }
}


#[derive(Default, Clone)]
struct TrackStats {
    total_frames: u64,
    missing_frames: u64,
    decoder_dropped: u64,
    last_sequence: Option<u64>,
    last_render: Option<Instant>,
    last_playback_position: Option<Duration>,
    frame_latency: RunningStats,
    jitter: RunningStats,
    stall_events: u64,
    total_stall_time: Duration,
    stall_durations: RunningStats,
    startup_delay: Option<Duration>,
    startup_delay_stats: RunningStats,
    vmaf: RunningStats,
    psnr: RunningStats,
    ssim: RunningStats,
}

impl TrackStats {
    fn to_report(&self, track: &str) -> TrackQoSReport {
        TrackQoSReport {
            track: track.to_string(),
            total_frames: self.total_frames,
            missing_frames: self.missing_frames,
            decoder_dropped: self.decoder_dropped,
            stall_events: self.stall_events,
            total_stall_time: self.total_stall_time,
            frame_latency: self.frame_latency.as_duration_summary(),
            playback_jitter: self.jitter.as_duration_summary(),
            stall_duration: self.stall_durations.as_duration_summary(),
            startup_delay: self.startup_delay,
            vmaf: self.vmaf.as_score_summary(),
            psnr: self.psnr.as_score_summary(),
            ssim: self.ssim.as_score_summary(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackQoSReport {
    pub track: String,
    pub total_frames: u64,
    pub missing_frames: u64,
    pub decoder_dropped: u64,
    pub stall_events: u64,
    pub total_stall_time: Duration,
    pub frame_latency: Option<DurationSummary>,
    pub playback_jitter: Option<DurationSummary>,
    pub stall_duration: Option<DurationSummary>,
    pub startup_delay: Option<Duration>,
    pub vmaf: Option<ScoreSummary>,
    pub psnr: Option<ScoreSummary>,
    pub ssim: Option<ScoreSummary>,
}

#[derive(Debug, Clone)]
pub struct DurationSummary {
    pub last: Duration,
    pub mean: Duration,
    pub min: Duration,
    pub max: Duration,
    pub samples: u64,
}

#[derive(Debug, Clone)]
pub struct ScoreSummary {
    pub last: f64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub samples: u64,
}

#[derive(Clone, Debug, Default)]
struct RunningStats {
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
    last: f64,
}

impl RunningStats {
    fn add(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.sum += value;
        self.count += 1;
        if self.count == 1 || value < self.min {
            self.min = value;
        }
        if self.count == 1 || value > self.max {
            self.max = value;
        }
        self.last = value;
    }

    fn add_duration(&mut self, value: Duration) {
        self.add(value.as_secs_f64());
    }

    fn add_seconds(&mut self, value: f64) {
        self.add(value);
    }

    fn mean(&self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.sum / self.count as f64)
        }
    }

    fn as_duration_summary(&self) -> Option<DurationSummary> {
        if self.count == 0 {
            return None;
        }
        let mean = self.mean().unwrap();
        Some(DurationSummary {
            last: Duration::from_secs_f64(self.last),
            mean: Duration::from_secs_f64(mean),
            min: Duration::from_secs_f64(self.min),
            max: Duration::from_secs_f64(self.max),
            samples: self.count,
        })
    }

    fn as_score_summary(&self) -> Option<ScoreSummary> {
        if self.count == 0 {
            return None;
        }
        let mean = self.mean().unwrap();
        Some(ScoreSummary {
            last: self.last,
            mean,
            min: self.min,
            max: self.max,
            samples: self.count,
        })
    }
}

