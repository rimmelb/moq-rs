use bytes::BytesMut;
use std::net;
use url::Url;

use anyhow::Context;
use clap::Parser;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

use tokio::time::sleep;

use moq_native_ietf::quic;
use moq_pub::Media;
use moq_transport::{
    coding::Tuple,
    serve::{self, TracksReader},
    session::SharedState,
};

use tracing_subscriber::{fmt, EnvFilter};
use tracing_subscriber::prelude::*;

fn init_tracing() {
    // RUST_LOG="bbr.deadline=debug,moq_transport=info,quinn=warn" cargo run -- ...
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            "bbr.sg=debug,moq_transport=info,quinn=warn,moq_native_ietf=info".parse().unwrap()
        });

    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_level(true)
        .with_ansi(false)
        .compact();

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}


#[derive(Parser, Clone)]
pub struct Cli {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Advertise this frame rate in the catalog (informational)
    // TODO auto-detect this from the input when not provided
    #[arg(long, default_value = "24")]
    pub fps: u8,

    /// Advertise this bit rate in the catalog (informational)
    // TODO auto-detect this from the input when not provided
    #[arg(long, default_value = "1500000")]
    pub bitrate: u32,

    /// Connect to the given URL starting with https://
    #[arg()]
    pub url: Url,

    /// The name of the broadcast
    #[arg(long)]
    pub name: String,

    /// Enable bandwidth monitoring and logging
    #[arg(long)]
    pub bandwidth_monitoring: bool,

    /// Rate limit for sending (bits per second). E.g., 1000000 for 1 Mbps
    #[arg(long)]
    pub rate_limit_bps: Option<u32>,

    /// Initial RTT hint in milliseconds for QUIC transport
    #[arg(long, value_name="MS")]
    pub initial_rtt_ms: Option<u32>,

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    //init_tracing();
    let mut cli = Cli::parse();
    let mut url = cli.url.clone();
    let (writer, _, reader) =
        Arc::new(serve::Tracks::new(Tuple::from_utf8_path(&cli.name))).produce();

    // Create media ONCE with the TracksWriter
    let media = Media::new(writer)?;
    let media_connector = Arc::new(Mutex::new(media));

    tokio::spawn({
        let media_connector = media_connector.clone();
        async move {
            if let Err(e) = run_media(media_connector).await {
                log::error!("Media task error: {}", e);
            }
        }
    });

    loop {
        // Pass TracksReader to announce, not trying to create from Publisher
        match connect_to_other_session(cli.clone(), url.clone(), reader.clone()).await {
            Ok(new_url) => {
                url = new_url;
                if let Some(port) = get_port(url.as_ref()) {
                    cli.bind.set_port(port);
                }
                break;
            }
            Err(e) => {
                log::error!("Connection failed: {}. Retrying in 5 seconds...", e);
                sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }

    Ok(())
}

fn get_port(url_str: &str) -> Option<u16> {
    Url::parse(url_str).ok()?.port()
}

async fn run_media(media: Arc<Mutex<Media>>) -> anyhow::Result<()> {
    let mut input = tokio::io::stdin();
    let mut buf = BytesMut::new();
    loop {
        input
            .read_buf(&mut buf)
            .await
            .context("failed to read from stdin")?;
        let mut media_guard = media.lock().await;
        media_guard
            .parse(&mut buf)
            .context("failed to parse media")?;
    }
}

async fn connect_to_other_session(cli: Cli, mut url: Url, r: TracksReader) -> anyhow::Result<Url> {
    loop {
        let tls = cli.tls.load()?;
        let quic = quic::Endpoint::new(moq_native_ietf::quic::Config {
            bind: cli.bind,
            tls: tls.clone(),
        },
        cli.rate_limit_bps,
        )?;

        log::info!("Connecting to relay: url={}", url);

        let (wt_session, raw_provider) = match quic.client.connect_with_stats(&url).await {
            Ok(x) => x,
            Err(e) => {
                log::error!("Connection failed: {}. Retrying...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        let provider: Option<Arc<dyn moq_transport::session::QuicStatsProvider + Send + Sync>> =
            raw_provider.map(|p| p as Arc<_>);

        // Create session and publisher with rate limiting support
        let (session, mut publisher) = if cli.rate_limit_bps.is_some() || provider.is_some() {
            moq_transport::session::Publisher::connect_with_stats_and_rate_limit(
                wt_session,
                provider, // trait-objektumként továbbadva
                cli.rate_limit_bps.map(|r| r as f64),
            )
            .await
            .context("failed to create MoQ Transport session with stats and rate limit")?
        } else {
            moq_transport::session::Publisher::connect(wt_session)
                .await
                .context("failed to create MoQ Transport session")?
        };

        if let Some(rate) = cli.rate_limit_bps {
            let rate = rate as f64;
            log::info!("Rate limiting enabled: {:.0} bps ({:.2} Mbps)", rate, rate / 1_000_000.0);
        }
        let reporter = session.media_qos_reporter.clone();
        let shared_state = SharedState::new();
        let result = tokio::select! {
            res = session.run(shared_state) => res.context("session error"),
            res = publisher.announce(r.clone(), reporter) => res.context("failed to serve tracks"),
        };

        match result {
            Ok(_) => {
                let url_str = publisher.get_url().await;
                url = Url::parse(&url_str).context("failed to parse URL")?;
                log::info!("New URL obtained: {}", url);
                return Ok(url);
            }
            Err(e) => {
                log::error!("Error occurred: {}. Retrying...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        }
    }
}
