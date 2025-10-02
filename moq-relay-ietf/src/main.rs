use clap::Parser;

mod api;
mod consumer;
mod local;
mod producer;
mod relay;
mod remote;
mod session;
mod web;

pub use api::*;
pub use consumer::*;
pub use local::*;
pub use producer::*;
pub use relay::*;
pub use remote::*;
pub use session::*;
pub use web::*;

use moq_transport::session::SharedState;
use std::net;
use url::Url;

#[derive(Parser, Clone)]
pub struct Cli {
    /// Listen on this address
    #[arg(long, default_value = "[::]:443")]
    pub bind: net::SocketAddr,

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,

    /// Forward all announces to the provided server for authentication/routing.
    /// If not provided, the relay accepts every unique announce.
    #[arg(long)]
    pub announce: Option<Url>,

    /// The URL of the moq-api server in order to run a cluster.
    /// Must be used in conjunction with --node to advertise the origin
    #[arg(long)]
    pub api: Option<Url>,

    /// The hostname that we advertise to other origins.
    /// The provided certificate must be valid for this address.
    #[arg(long)]
    pub node: Option<Url>,

    /// Enable development mode.
    /// This hosts a HTTPS web server via TCP to serve the fingerprint of the certificate.
    #[arg(long)]
    pub dev: bool,

    /// Enable bandwidth monitoring with reporting interval in seconds
    #[arg(long)]
    pub bandwidth_monitoring: Option<u64>,

    /// Set a global rate limit in bits per second
    #[arg(long)]
    pub rate_limit_bps: Option<u32>,

    /// Initial RTT hint in milliseconds for QUIC transport
    #[arg(long, value_name="MS")]
    pub initial_rtt_ms: Option<u32>,

    #[arg(long)]
    pub delivery_timeout: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Disable tracing so we don't get a bunch of Quinn spam.
    let tracer = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(tracer).unwrap();

    let cli = Cli::parse();
    let tls = cli.tls.load()?;

    if tls.server.is_none() {
        anyhow::bail!("missing TLS certificates");
    }

    let shared_state = SharedState::new();
    let relay_stopping_state = SharedState::new();

    // Create a QUIC server for media.
    let relay = Relay::new(
        RelayConfig {
            tls: tls.clone(),
            bind: cli.bind,
            node: cli.node,
            api: cli.api,
            announce: cli.announce,
            bandwidth_monitoring: cli.bandwidth_monitoring,
            rate_limit_bps: cli.rate_limit_bps,
            rtt_ms: cli.initial_rtt_ms,
            delivery_timeout: cli.delivery_timeout
        },
        shared_state.clone(),
        relay_stopping_state.clone(),
    )?;

    if let Some(rate) = cli.rate_limit_bps {
        let rate = rate as f64;
        log::info!("Global rate limit enabled: {:.0} bps ({:.2} Mbps)", rate, rate / 1_000_000.0);
    }

    if cli.dev {
        // Create a web server too.
        // Currently this only contains the certificate fingerprint (for development only).
        let web = Web::new(WebConfig {
            bind: cli.bind,
            tls,
            shared_state: shared_state.clone(),
            relay_stopping_state: relay_stopping_state.clone(),
        });
        tokio::spawn(async move {
            web.run().await.expect("failed to run web server");
        });
    }

    relay.run().await
}
