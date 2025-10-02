// use crate::session; // unused
// session.rs
use crate::{Consumer, Producer};
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::session::SessionError;
use moq_transport::session::SharedState;

pub struct Session {
    pub session: moq_transport::session::Session,
    pub producer: Option<Producer>,
    pub consumer: Option<Consumer>,
}

impl Session {
    pub async fn run(self, shared_state: SharedState, delivery_timeout: Option<u64>) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        tasks.push(self.session.run(shared_state.clone()).boxed());

        if let Some(producer) = self.producer {
            tasks.push(producer.run().boxed());
        }

        if let Some(consumer) = self.consumer {
            tasks.push(consumer.run(delivery_timeout.clone()).boxed());
        }
        tasks.select_next_some().await
    }

    /// Print current bandwidth statistics
    pub async fn print_bandwidth_stats(&self) {
        let recv_bw = self.session.recv_bandwidth_mbps().await;
        let send_bw = self.session.send_bandwidth_mbps().await;

        println!("=== Bandwidth Statistics ===");
        println!("Receive: {:.2} Mbps ({:.2} KB/s)",
            recv_bw,
            self.session.recv_bandwidth_bps().await / 8000.0
        );
        println!("Send: {:.2} Mbps ({:.2} KB/s)",
            send_bw,
            self.session.send_bandwidth_bps().await / 8000.0
        );
        println!("============================");
    }

    /// Run with periodic bandwidth reporting
    pub async fn run_with_bandwidth_monitoring(self, shared_state: SharedState, report_interval_secs: u64, rate_limit: f64, delivery_timeout: Option<u64>) -> Result<(), SessionError> {
        // Clone the bandwidth estimators before moving self.session
        let recv_estimator = self.session.recv_bandwidth_estimator.clone();
        let send_estimator = self.session.send_bandwidth_estimator.clone();

        let bandwidth_reporter = async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(report_interval_secs));
            log::info!("Bandwidth monitoring task started with interval: {}s", report_interval_secs);
            log::info!("Rate limit for session: {:.0} bps ({:.2} Mbps)", rate_limit, rate_limit / 1_000_000.0);

            loop {
                interval.tick().await;

                let recv_mbps = {
                    let estimator = recv_estimator.lock().await;
                    estimator.bandwidth_mbps()
                };

                let send_mbps = {
                    let estimator = send_estimator.lock().await;
                    estimator.bandwidth_mbps()
                };

                // Show additional bandwidth metrics
                let recv_bps = recv_mbps * 1_000_000.0;
                let send_bps = send_mbps * 1_000_000.0;

                if recv_bps > 0.0 || send_bps > 0.0 {
                    log::info!("[BANDWIDTH DETAIL] Recv: {:.0} bps ({:.1} KB/s) | Send: {:.0} bps ({:.1} KB/s)",
                        recv_bps, recv_bps / 8000.0, send_bps, send_bps / 8000.0);
                }

                // Also show bytes since last sample for debugging
                let _recv_bytes = {
                    let estimator = recv_estimator.lock().await;
                    estimator.bytes_since_last_sample()
                };

                let _send_bytes = {
                    let estimator = send_estimator.lock().await;
                    estimator.bytes_since_last_sample()
                };

                // Always show byte counts to see if any data is flowing
                //log::info!("[BANDWIDTH DEBUG] Recv bytes: {}, Send bytes: {}", recv_bytes, send_bytes);
            }
        };

        let mut tasks = FuturesUnordered::new();
        tasks.push(self.session.run(shared_state.clone()).boxed());
        tasks.push(bandwidth_reporter.boxed());

        if let Some(producer) = self.producer {
            tasks.push(producer.run().boxed());
        }

        if let Some(consumer) = self.consumer {
            tasks.push(consumer.run(delivery_timeout.clone()).boxed());
        }

        tasks.select_next_some().await
    }
}
