use std::net;
use std::sync::Arc;

use anyhow::Context;

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_native_ietf::quic;
use moq_transport::session::SharedState;
use url::Url;

use crate::{Api, Consumer, Locals, Producer, Remotes, RemotesConsumer, RemotesProducer, Session};

pub struct RelayConfig {
    /// Listen on this address
    pub bind: net::SocketAddr,

    /// The TLS configuration.
    pub tls: moq_native_ietf::tls::Config,

    /// Forward all announcements to the (optional) URL.
    pub announce: Option<Url>,

    /// Connect to the HTTP moq-api at this URL.
    pub api: Option<Url>,

    /// Our hostname which we advertise to other origins.
    /// We use QUIC, so the certificate must be valid for this address.
    pub node: Option<Url>,

    /// Bandwidth monitoring interval in seconds (None = disabled)
    pub bandwidth_monitoring: Option<u64>,

    /// Rate limit for outgoing connections (bits per second)
    pub rate_limit_bps: Option<u32>,

    /// Initial RTT hint in milliseconds for QUIC transport
    pub rtt_ms: Option<u32>, // <- NEW

    /// Delivery timeout in seconds for the relay to wait for a consumer to connect
    pub delivery_timeout: Option<u64>
}

pub struct Relay {
    quic: quic::Endpoint,
    announce: Option<Url>,
    locals: Locals,
    api: Option<Api>,
    remotes: Option<(RemotesProducer, RemotesConsumer)>,
    shared_state: SharedState,
    relay_stopping_state: SharedState,
    bandwidth_monitoring: Option<u64>,
    rate_limit_bps: Option<u32>, // új mező
    delivery_timeout: Option<u64>, // új mező
}

//for Goaway -> curl -X POST "https://localhost:4443/goaway?url=https://localhost:4442&timeout=5"

impl Relay {
    // ITT lehet állítani az RTT értékét
    // Create a QUIC endpoint that can be used for both clients and servers.
    pub fn new(
        config: RelayConfig,
        shared_state: SharedState,
        relay_stopping_state: SharedState,
    ) -> anyhow::Result<Self> {
        let quic = quic::Endpoint::new(quic::Config {
            bind: config.bind,
            tls: config.tls,
        },
        config.rate_limit_bps,
        config.rtt_ms,
        )?;

        let api = if let (Some(url), Some(node)) = (config.api, config.node) {
            log::info!("using moq-api: url={} node={}", url, node);
            Some(Api::new(url, node))
        } else {
            None
        };

        let locals = Locals::new();

        let remotes = api.clone().map(|api| {
            Remotes {
                api,
                quic: quic.client.clone(),
            }
            .produce()
        });

        Ok(Self {
            quic,
            announce: config.announce,
            api,
            locals,
            remotes,
            shared_state,
            relay_stopping_state,
            bandwidth_monitoring: config.bandwidth_monitoring,
            rate_limit_bps: config.rate_limit_bps, // új mező
            delivery_timeout: config.delivery_timeout, // új mező
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let mut tasks = FuturesUnordered::new();

        let remotes = self.remotes.map(|(producer, consumer)| {
            tasks.push(producer.run().boxed());
            consumer
        });
        let mut provider: Option<Arc<dyn moq_transport::session::QuicStatsProvider + Send + Sync>> = None;

        // Forward session
        let forward = if let Some(url) = &self.announce {
            log::info!("forwarding announces to {}", url);
            let (session, raw_provider) = self
                .quic
                .client
                .connect_with_stats(url)
                .await
                .context("failed to establish forward connection")?;

            provider = raw_provider.map(|p| p as Arc<_>);

            let (mut session, publisher, subscriber) = if let Some(rate) = self.rate_limit_bps {
                let rate = rate as f64;
                log::info!(
                    "Forward session rate limit: {:.0} bps ({:.2} Mbps)",
                    rate,
                    rate / 1_000_000.0
                );
                moq_transport::session::Session::connect_role_with_rate_limit(
                    session,
                    moq_transport::setup::Role::Both,
                    provider.clone(),
                    Some(rate),
                )
                .await
                .context("failed to establish forward session with rate limit")?
            } else {
                moq_transport::session::Session::connect_role_with_stats(
                    session,
                    moq_transport::setup::Role::Both,
                    provider.clone(),
                )
                .await
                .context("failed to establish forward session")?
            };

            let session = Session {
                session,
                producer: publisher.map(|publisher| Producer::new(
                    publisher,
                    self.locals.clone(),
                    remotes.clone(),
                )),
                consumer: subscriber.map(|subscriber| Consumer::new(subscriber, self.locals.clone(), None, None)),
            };
            let shared_state = self.shared_state.clone();
            let forward = session.producer.clone();
            tasks.push(
                async move { session.run(shared_state, self.delivery_timeout.clone()).await.context("forwarding failed") }.boxed(),
            );

            forward
        } else {
            None
        };

        let mut server = self.quic.server.context("missing TLS certificate")?;
        log::info!("listening on {}", server.local_addr()?);
        let shared_state = self.shared_state.clone();
        let relay_stopping_state = self.relay_stopping_state.clone();

        // Clone néhány értéket a loop előtt
        let locals = self.locals.clone();
        let api = self.api.clone();
        let bandwidth_monitoring = self.bandwidth_monitoring;

        loop {
            tokio::select! {
                res = server.accept_with_stats() => {
                    let (conn_session, raw_provider) =
                    res.context("failed to accept QUIC connection")?;
                    let provider: Option<
                    Arc<dyn moq_transport::session::QuicStatsProvider + Send + Sync>
                    > = raw_provider.map(|p| p as Arc<_>);

                    let rate_limit = self.rate_limit_bps;
                    let locals = locals.clone();
                    let api = api.clone();
                    let remotes = remotes.clone();
                    let forward = forward.clone();
                    let shared_state = shared_state.clone();

                    tasks.push(async move {
                        let (mut session, publisher, subscriber) =
                            moq_transport::session::Session::accept_with_stats(conn_session, provider)
                                .await
                                .context("failed to accept MoQ session")?;

                        if let Some(rate) = rate_limit {
                            // Set fixed send bandwidth in Mbps, then propagate to Publisher
                            let rate = rate as f64;
                            session.set_fixed_send_bandwidth_mbps(Some(rate / 1_000_000.0));
                            session.apply_send_rate_limit_to_publisher();
                            log::info!("Applied relay rate limit to session: {:.0} bps ({:.2} Mbps)", rate, rate/1_000_000.0);
                        }

                        let session = Session {
                            session,
                            producer: publisher.map(|publisher| Producer::new(publisher, locals.clone(), remotes)),
                            consumer: subscriber.map(|subscriber| Consumer::new(subscriber, locals, api, forward)),
                        };

                        // Use bandwidth monitoring if configured
                         if let Some(interval) = bandwidth_monitoring {
                            log::info!("Starting session with bandwidth monitoring (interval: {}s)", interval);
                            let rate_limit = rate_limit.map(|r| r as f64);
                            let effective_rate = rate_limit.unwrap_or(0.0); // 0.0 = no cap
                            if let Err(err) = session.run_with_bandwidth_monitoring(shared_state, interval, effective_rate, self.delivery_timeout.clone()).await {
                                log::warn!("failed to run MoQ session with bandwidth monitoring: {}", err);
                            }
                        } else {
                            if let Err(err) = session.run(shared_state, self.delivery_timeout.clone()).await {
                                log::warn!("failed to run MoQ session: {}", err);
                            }
                        }

                        Ok::<(), anyhow::Error>(())
                    }.boxed());
                },
                res = tasks.next(), if !tasks.is_empty() => res.unwrap()?,
            }
        }
    }
}
