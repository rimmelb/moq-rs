use std::time::{Duration, Instant};
use std::collections::VecDeque;

/// Cross-layer metrics collected from QUIC transport layer
/// Used for advanced congestion control and deadline-aware scheduling
#[derive(Debug, Clone)]
pub struct CrossLayerMetrics {
    /// Current bandwidth estimate from application layer (Westwood)
    pub bandwidth_estimate_bps: f64,

    /// Minimum RTT observed (smoothed)
    pub rtt_min: Duration,

    /// Current RTT estimate
    pub rtt_current: Duration,

    /// Current congestion window size in bytes
    pub cwnd: u64,

    /// Packet loss rate (0.0 - 1.0)
    pub packet_loss_rate: f64,

    /// In-flight bytes (unacknowledged)
    pub bytes_in_flight: u64,

    /// Packets per second capacity for this path
    pub packets_per_second: f64,

    /// MSS (Maximum Segment Size)
    pub mss: u32,

    /// Queue length estimation
    pub queue_length: u32,

    /// Last update timestamp
    pub last_update: Instant,
}

impl Default for CrossLayerMetrics {
    fn default() -> Self {
        Self {
            bandwidth_estimate_bps: 0.0,
            rtt_min: Duration::from_millis(10), // Conservative minimum
            rtt_current: Duration::from_millis(50),
            cwnd: 10000, // Default CWND
            packet_loss_rate: 0.0,
            bytes_in_flight: 0,
            packets_per_second: 0.0,
            mss: 1200, // Conservative MTU
            queue_length: 0,
            last_update: Instant::now(),
        }
    }
}

impl CrossLayerMetrics {
    /// Calculate link utilization capacity based on bandwidth estimate and RTT
    /// Returns capacity in bytes
    pub fn link_capacity(&self) -> u64 {
        // Capacity = BandwidthEstimate * RTTmin (in bytes)
        let capacity_bits = self.bandwidth_estimate_bps * self.rtt_min.as_secs_f64();
        (capacity_bits / 8.0) as u64
    }

    /// Calculate packets per second for this path
    /// pps = ⌊CWND/MSS⌋ / RTT
    pub fn calculate_packets_per_second(&self) -> f64 {
        let packets_in_cwnd = (self.cwnd / self.mss as u64) as f64;
        packets_in_cwnd / self.rtt_current.as_secs_f64()
    }

    /// Check if CWND should be reduced based on cross-layer information
    /// Returns true if congestion is likely due to network capacity limitations
    pub fn should_reduce_cwnd(&self) -> bool {
        let link_cap = self.link_capacity();

        // If current CWND is significantly larger than link capacity,
        // and we have packet loss, it's likely congestion-based
        self.cwnd > link_cap && self.packet_loss_rate > 0.01 // 1% loss threshold
    }

    /// Deadline-aware path suitability check
    /// Returns true if this path can deliver a block within the deadline
    pub fn is_path_suitable(&self, block_size_bytes: u64, deadline: Duration) -> bool {
        let packets_needed = (block_size_bytes + self.mss as u64 - 1) / self.mss as u64;
        let pps = self.calculate_packets_per_second();

        if pps <= 0.0 {
            return false;
        }

        // Transmission time = RTT/2 + (queue + packets_needed) / pps
        let transmission_time = self.rtt_current.as_secs_f64() / 2.0
            + (self.queue_length as f64 + packets_needed as f64) / pps;

        Duration::from_secs_f64(transmission_time) < deadline
    }
}

/// Manager for cross-layer metrics integration
/// Provides advanced congestion control algorithms based on multiple network layers
#[derive(Debug)]
pub struct CrossLayerManager {
    current_metrics: CrossLayerMetrics,
    rtt_history: VecDeque<Duration>,
    loss_history: VecDeque<f64>,
    max_history: usize,
}

impl CrossLayerManager {
    pub fn new() -> Self {
        Self {
            current_metrics: CrossLayerMetrics::default(),
            rtt_history: VecDeque::new(),
            loss_history: VecDeque::new(),
            max_history: 100, // Keep last 100 samples
        }
    }

    /// Update metrics from QUIC connection stats
    /// This should be called on every ACK or at regular intervals
    pub fn update_from_quic(&mut self,
        rtt: Duration,
        cwnd: u64,
        bytes_in_flight: u64,
        packets_lost: u64,
        packets_sent: u64
    ) {
        // Update RTT tracking
        self.rtt_history.push_back(rtt);
        if self.rtt_history.len() > self.max_history {
            self.rtt_history.pop_front();
        }

        // Calculate minimum RTT
        self.current_metrics.rtt_min = self.rtt_history.iter()
            .min()
            .copied()
            .unwrap_or(Duration::from_millis(10));

        self.current_metrics.rtt_current = rtt;
        self.current_metrics.cwnd = cwnd;
        self.current_metrics.bytes_in_flight = bytes_in_flight;

        // Calculate packet loss rate
        if packets_sent > 0 {
            let loss_rate = packets_lost as f64 / packets_sent as f64;
            self.loss_history.push_back(loss_rate);
            if self.loss_history.len() > self.max_history {
                self.loss_history.pop_front();
            }

            // Smooth loss rate over recent history
            self.current_metrics.packet_loss_rate = self.loss_history.iter()
                .sum::<f64>() / self.loss_history.len() as f64;
        }

        // Update packets per second
        self.current_metrics.packets_per_second = self.current_metrics.calculate_packets_per_second();

        self.current_metrics.last_update = Instant::now();
    }

    /// Update bandwidth estimate from application layer
    pub fn update_bandwidth_estimate(&mut self, bandwidth_bps: f64) {
        self.current_metrics.bandwidth_estimate_bps = bandwidth_bps;
        self.current_metrics.packets_per_second = self.current_metrics.calculate_packets_per_second();
    }

    /// Get current cross-layer metrics
    pub fn get_metrics(&self) -> &CrossLayerMetrics {
        &self.current_metrics
    }

    /// Get mutable reference for advanced operations
    pub fn get_metrics_mut(&mut self) -> &mut CrossLayerMetrics {
        &mut self.current_metrics
    }
}
