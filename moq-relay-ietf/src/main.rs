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

use tracing_subscriber::{fmt, EnvFilter};
use tracing_subscriber::prelude::*;

fn init_tracing() {
    // RUST_LOG-al felülírható, pl.:
    // RUST_LOG="bbr.deadline=debug,moq_relay_ietf=info,moq_transport=debug" ./dev/relay
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            // Alapértelmezett: bbr.deadline és bbr.sg=debug, relay/transport=info, Quinn=warn
            "bbr.deadline=debug,bbr.sg=debug,moq_relay_ietf=info,moq_transport=info,quinn=warn,moq_native_ietf=info"
                .parse()
                .unwrap()
        });

    let fmt_layer = fmt::layer()
        .with_target(true)       // Mutassa a target-et (modul név)
        .with_thread_ids(false)  // Ne mutassa a thread ID-t
        .with_level(true)        // Mutassa a log szintet
        .with_ansi(false)        // Ne használjon színeket (ha fájlba megy)
        .compact();              // Kompakt formátum

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}


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
    pub rate_limit_mbps: Option<u32>,

    /// Initial RTT hint in milliseconds for QUIC transport
    #[arg(long, value_name="MS")]
    pub initial_rtt_ms: Option<u32>,

    #[arg(long)]
    pub delivery_timeout: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

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
            rate_limit_bps: cli.rate_limit_mbps,
            rtt_ms: cli.initial_rtt_ms,
            delivery_timeout: cli.delivery_timeout
        },
        shared_state.clone(),
        relay_stopping_state.clone(),
    )?;

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
