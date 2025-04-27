use bytes::BytesMut;
use std::net;
use url::Url;

use anyhow::Context;
use clap::Parser;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use std::sync::Arc;


use tokio::time::sleep;

use moq_native_ietf::quic;
use moq_pub::Media;
use moq_transport::{coding::Tuple, serve::{self, TracksReader}, session::Publisher, session::SharedState};

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

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Disable tracing so we don't get a bunch of Quinn spam.
    let tracer = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(tracer).unwrap();

    let mut cli = Cli::parse();
    let mut url = cli.url.clone();
    let (writer, _, reader) = Arc::new(serve::Tracks::new(Tuple::from_utf8_path(&cli.name))).produce();
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
        match connect_to_other_session(cli.clone(), url.clone(), reader.clone()).await {
            Ok(new_url) => {
                url = new_url;
                if let Some(port) = get_port(&url.to_string()) {
                    cli.bind.set_port(port);
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


async fn run_media(media: Arc<Mutex<Media>>) -> anyhow::Result<()> {
    let mut input = tokio::io::stdin();
    let mut buf = BytesMut::new();
    loop {
        input
            .read_buf(&mut buf)
            .await
            .context("failed to read from stdin")?;
        let mut media_guard = media.lock().await;
        media_guard.parse(&mut buf).context("failed to parse media")?;
    }
}

async fn connect_to_other_session(cli: Cli, mut url: Url, r: TracksReader) -> anyhow::Result<Url> {
    loop {
        let tls = cli.tls.load()?;
        let quic = quic::Endpoint::new(moq_native_ietf::quic::Config {
            bind: cli.bind,
            tls: tls.clone(),
        })?;

        log::info!("Connecting to relay: url={}", url);
        let session = match quic.client.connect(&url).await {
            Ok(session) => session,
            Err(e) => {
                log::error!("Failed to connect to relay: {}. Retrying...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let (session, mut publisher) = match Publisher::connect(session).await {
            Ok(publisher) => publisher,
            Err(e) => {
                log::error!("Failed to create MoQ Transport publisher: {}. Retrying...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let shared_state = SharedState::new();

        let result = tokio::select! {
            res = session.run(shared_state) => res.context("session error"),
            res = publisher.announce(r.clone()) => res.context("publisher error"),
        };

        match result {
            Ok(_) => return Ok(url),
            Err(e) => {
                log::error!("Error occurred: {}. Fetching new URL from publisher...", e);
                url = Url::parse(&publisher.get_url().await)
                    .context("failed to parse URL")?;
                log::info!("New URL obtained: {}", url);
            }
        }
    }
}
