use std::time::{Duration, Instant};

use crate::util::{CrossLayerManager, CrossLayerMetrics};

/// Real-time bandwidth estimator based on the Westwood algorithm approach
/// with integrated cross-layer metrics for advanced congestion control
///
/// This estimator tracks bandwidth by:
/// 1. Measuring bytes received since the last sample (Δbytes)
/// 2. Dividing by elapsed time (Δt) to get instant rate: Δbytes/Δt
/// 3. Smoothing with exponential average (smoothing factor: 7/8)
///    Best ← 0.875 * Best + 0.125 * (Δbytes/Δt)
/// 4. Storing the result in bits/s
/// 5. Integrating with cross-layer metrics for smart congestion control
#[derive(Debug)]
pub struct BandwidthEstimator {
    /// Current bandwidth estimate in bits per second
    estimated_bandwidth_bps: f64,

    /// Total bytes processed since the last measurement
    bytes_since_last_sample: u64,

    /// Timestamp of the last bandwidth calculation
    last_sample_time: Instant,

    /// Smoothing parameter (default: 7/8 = 0.875)
    smoothing_factor: f64,

    /// Minimum time interval between samples to avoid noise
    min_sample_interval: Duration,

    /// Cross-layer metrics manager for advanced congestion control
    cross_layer_manager: Option<CrossLayerManager>,

    /// Enable cross-layer metrics collection
    cross_layer_enabled: bool,
}

impl Default for BandwidthEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl BandwidthEstimator {
    /// Create a new bandwidth estimator with default parameters
    pub fn new() -> Self {
        Self {
            estimated_bandwidth_bps: 0.0,
            bytes_since_last_sample: 0,
            last_sample_time: Instant::now(),
            smoothing_factor: 7.0 / 8.0, // 0.875
            min_sample_interval: Duration::from_millis(100), // 100ms minimum
            cross_layer_manager: None,
            cross_layer_enabled: false,
        }
    }

    /// Create a new bandwidth estimator with cross-layer metrics enabled
    pub fn with_cross_layer() -> Self {
        Self {
            estimated_bandwidth_bps: 0.0,
            bytes_since_last_sample: 0,
            last_sample_time: Instant::now(),
            smoothing_factor: 7.0 / 8.0,
            min_sample_interval: Duration::from_millis(100),
            cross_layer_manager: Some(CrossLayerManager::new()),
            cross_layer_enabled: true,
        }
    }

    /// Record bytes received/sent
    /// This should be called every time data is processed
    pub fn record_bytes(&mut self, bytes: u64) {
        self.bytes_since_last_sample += bytes;
    }

    /// Update bandwidth estimate if enough time has passed
    /// Returns the updated bandwidth in bits/s if an update occurred
    pub fn update(&mut self) -> Option<f64> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample_time);

        // Only update if minimum interval has passed
        if elapsed < self.min_sample_interval {
            return None;
        }

        // Calculate instant bandwidth rate
        let elapsed_secs = elapsed.as_secs_f64();
        if elapsed_secs > 0.0 && self.bytes_since_last_sample > 0 {
            // Convert bytes to bits and calculate rate
            let instant_rate_bps = (self.bytes_since_last_sample as f64 * 8.0) / elapsed_secs;

            // Apply exponential smoothing: Best ← α * Best + (1-α) * instant_rate
            // where α = smoothing_factor (7/8), so (1-α) = 1/8 = 0.125
            self.estimated_bandwidth_bps = self.smoothing_factor * self.estimated_bandwidth_bps
                + (1.0 - self.smoothing_factor) * instant_rate_bps;

            // Reset for next measurement
            self.bytes_since_last_sample = 0;
            self.last_sample_time = now;

            Some(self.estimated_bandwidth_bps)
        } else {
            None
        }
    }

    /// Get current bandwidth estimate in bits per second
    pub fn bandwidth_bps(&self) -> f64 {
        self.estimated_bandwidth_bps
    }

    /// Get current bandwidth estimate in kilobits per second
    pub fn bandwidth_kbps(&self) -> f64 {
        self.estimated_bandwidth_bps / 1000.0
    }

    /// Get current bandwidth estimate in megabits per second
    pub fn bandwidth_mbps(&self) -> f64 {
        self.estimated_bandwidth_bps / 1_000_000.0
    }

    /// Update QUIC metrics for cross-layer analysis
    pub fn update_quic_metrics(&mut self,
        rtt: Duration,
        cwnd: u64,
        bytes_in_flight: u64,
        packets_lost: u64,
        packets_sent: u64
    ) {
        if let Some(ref mut manager) = self.cross_layer_manager {
            manager.update_from_quic(rtt, cwnd, bytes_in_flight, packets_lost, packets_sent);
        }
    }

    /// Get current cross-layer metrics if available
    pub fn cross_layer_metrics(&self) -> Option<CrossLayerMetrics> {
        self.cross_layer_manager.as_ref().map(|m| m.get_metrics().clone())
    }

    /// Get bytes accumulated since last sample
    pub fn bytes_since_last_sample(&self) -> u64 {
        self.bytes_since_last_sample
    }

    /// Reset the estimator to initial state
    pub fn reset(&mut self) {
        self.estimated_bandwidth_bps = 0.0;
        self.bytes_since_last_sample = 0;
        self.last_sample_time = Instant::now();
    }
}
