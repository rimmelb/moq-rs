use std::net;

use anyhow::Context;
use clap::Parser;
use url::Url;

use moq_native_ietf::quic;
use moq_sub::media::Media;
use moq_transport::{coding::Tuple, serve::Tracks, session::SharedState};

use std::sync::Arc;
use tokio::time::sleep;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Disable tracing so we don't get a bunch of Quinn spam.
    let tracer = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(tracer).unwrap();

    let mut config = Config::parse();
    let mut url = config.url.clone();
    let tracks = Arc::new(Tracks::new(Tuple::from_utf8_path(&config.name)));

    loop {
        match connect_to_other_session(config.clone(), url.clone(), tracks.clone()).await {
            Ok(new_url) => {
                url = new_url;
                if let Some(port) = get_port(url.as_ref()) {
                    config.bind.set_port(port);
                }
                break;
            }
            Err(e) => {
                log::error!("Error occurred: {}. Retrying...", e);
                sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }

    Ok(())
}

fn get_port(url_str: &str) -> Option<u16> {
    Url::parse(url_str).ok()?.port()
}

async fn connect_to_other_session(
    config: Config,
    mut url: Url,
    t: Arc<Tracks>,
) -> anyhow::Result<Url> {
    loop {
        let out = tokio::io::stdout();
        let tls = config.tls.load()?;
        let quic = quic::Endpoint::new(
            quic::Config {
                bind: config.bind,
                tls,
            },
            config.rate_limit_bps,      // was: Some(3 * 1000000)
            config.initial_rtt_ms,      // was: Some(50)
        )?;

        // Quinn session + provider
        let (wt_session, provider) = quic.client.connect_with_stats(&url).await?;
        let provider: Option<Arc<dyn moq_transport::session::QuicStatsProvider + Send + Sync>> =
            provider.map(|p| p as Arc<_>);

        // Hozz létre Subscriber session-t rate limit támogatással
        let (session, subscriber) = {
            let (session, _, sub_opt) = moq_transport::session::Session::connect_role_with_rate_limit(
                wt_session,
                moq_transport::setup::Role::Subscriber,
                provider,
                config.rate_limit_bps.map(|r| r as f64),
            ).await?;
            (session, sub_opt.expect("subscriber role"))
        };

        if let Some(rate) = config.rate_limit_bps {
            let rate = rate as f64;
            log::info!("SUB rate limit enabled: {:.0} bps ({:.2} Mbps)", rate, rate/1_000_000.0);
        }
        if let Some(rtt) = config.initial_rtt_ms {
            log::info!("SUB initial RTT hint: {} ms", rtt);
        }

        let mut media = Media::new(subscriber.clone(), t.clone(), out).await?;
        let shared_state = SharedState::new();

        let result = tokio::select! {
            res = session.run(shared_state) => res.context("session error"),
            res = media.run() => res.context("media error"),
        };

        match result {
            Ok(_) => return Ok(url),
            Err(e) => {
                log::error!("Error occurred: {}. Retrying...", e);
                match Url::parse(&subscriber.get_url()) {
                    Ok(new_url) => {
                        if new_url != url {
                            log::info!("Switching to new URL: {}", new_url);
                            url = new_url;
                        }
                    }
                    Err(parse_err) => {
                        log::error!(
                            "Failed to parse new URL from subscriber: {}. Keeping current URL: {}",
                            parse_err,
                            url
                        );
                    }
                }
                sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

#[derive(Parser, Clone)]
pub struct Config {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Connect to the given URL starting with https://
    #[arg(value_parser = moq_url)]
    pub url: Url,

    /// The name of the broadcast
    #[arg(long)]
    pub name: String,

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,

    /// Set a client-side rate limit (bps) for SUB's outgoing writes (control)
    #[arg(long)]
    pub rate_limit_bps: Option<u32>,

    /// Initial RTT hint in milliseconds for QUIC (passed to transport)
    #[arg(long, value_name="MS")]
    pub initial_rtt_ms: Option<u32>,
}

fn moq_url(s: &str) -> Result<Url, String> {
    let url = Url::try_from(s).map_err(|e| e.to_string())?;

    // Make sure the scheme is moq
    if url.scheme() != "https" && url.scheme() != "moqt" {
        return Err("url scheme must be https:// for WebTransport & moqt:// for QUIC".to_string());
    }
    Ok(url)
}
