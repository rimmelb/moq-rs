use std::{net, sync::Arc, time, time::Duration};
use futures::future::BoxFuture;
use futures::FutureExt;
use moq_transport::session::QuicStatsProvider; // transport-side trait

use anyhow::Context;
use clap::Parser;
use url::Url;

use crate::tls;
use quinn::VarInt;

use futures::stream::{FuturesUnordered, StreamExt};

#[derive(Parser, Clone)]
pub struct Args {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    #[command(flatten)]
    pub tls: tls::Args,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            bind: "[::]:0".parse().unwrap(),
            tls: Default::default(),
        }
    }
}

impl Args {
    pub fn load(&self) -> anyhow::Result<Config> {
        let tls = self.tls.load()?;
        Ok(Config {
            bind: self.bind,
            tls,
        })
    }
}

pub struct Config {
    pub bind: net::SocketAddr,
    pub tls: tls::Config,
}

pub struct Endpoint {
    pub client: Client,
    pub server: Option<Server>,
}

pub struct QuinnStatsProvider {
    pub conn: quinn::Connection,
}

impl QuinnStatsProvider {
    pub fn new(conn: quinn::Connection) -> Self {
        Self { conn }
    }
    pub fn get_connection(&self) -> quinn::Connection {
        self.conn.clone()
    }
}
impl QuicStatsProvider for QuinnStatsProvider {
    fn get_stats(&self) -> BoxFuture<'_, Option<(Duration, u64, u64, u64, u64)>> {
        let conn = self.conn.clone();
        async move {
            // conn.stats() közvetlenül ConnectionStats-ot ad vissza, nem Result-ot
            let stats = conn.stats();

            // RTT elérhető stats.path.rtt alatt
            let rtt = stats.path.rtt;

            // Fallback értékek a többi mezőhöz (amíg pontosabb mapping nincs)
            let cwnd = stats.path.cwnd;
            let bytes_in_flight = stats.path.congestion_events;
            let packets_lost = stats.path.lost_packets;
            let packets_sent = stats.path.sent_packets;

            Some((rtt, cwnd, bytes_in_flight, packets_lost, packets_sent))
        }
        .boxed()
    }
    fn get_connection(&self) -> quinn::Connection {
        self.conn.clone()
    }
}

impl Endpoint {
    pub fn new(config: Config, rate_limit: Option<u32>, rtt: Option<u32>) -> anyhow::Result<Self> {
        // Enable BBR congestion control
        // TODO validate the implementation
        let mut transport = quinn::TransportConfig::default();

    transport
        .max_idle_timeout(Some(time::Duration::from_secs(30).try_into().unwrap()))
        .keep_alive_interval(Some(time::Duration::from_secs(10)))
        .enable_segmentation_offload(false)            // GSO off → kevesebb micro-burst
        .mtu_discovery_config(None)                    // MTU discovery off
        .initial_mtu(1200)
        .min_mtu(1200);

    if let Some(rtt_ms) = rtt {
        transport.initial_rtt(time::Duration::from_millis(rtt_ms as u64));
    }

    // ---- BBR hard-cap ----
    let mut bbr = quinn::congestion::BbrConfig::default()
        .enable_deadline_scheduler(false)
        .beta(0.8)
        .guard_ms(10)
        .default_mss(1200);

    transport.congestion_controller_factory(Arc::new(bbr));


        let transport = Arc::new(transport);

        let mut server_config = None;

        if let Some(mut config) = config.tls.server {
            config.alpn_protocols = vec![
                web_transport_quinn::ALPN.as_bytes().to_vec(),
                moq_transport::setup::ALPN.to_vec(),
            ];
            config.key_log = Arc::new(rustls::KeyLogFile::new());

            let config: quinn::crypto::rustls::QuicServerConfig = config.try_into()?;
            let mut config = quinn::ServerConfig::with_crypto(Arc::new(config));
            config.transport_config(transport.clone());

            server_config = Some(config);
        }

        // There's a bit more boilerplate to make a generic endpoint.
        let runtime = quinn::default_runtime().context("no async runtime")?;
        let endpoint_config = quinn::EndpointConfig::default();
        let socket = std::net::UdpSocket::bind(config.bind).context("failed to bind UDP socket")?;

        // Create the generic QUIC endpoint.
        let quic = quinn::Endpoint::new(endpoint_config, server_config.clone(), socket, runtime)
            .context("failed to create QUIC endpoint")?;

        let server = server_config.is_some().then(|| Server {
            quic: quic.clone(),
            accept: Default::default(),
        });

        let client = Client {
            quic,
            config: config.tls.client,
            transport,
        };

        Ok(Self { client, server })
    }
}

pub struct Server {
    quic: quinn::Endpoint,
    accept: FuturesUnordered<BoxFuture<'static, anyhow::Result<web_transport::Session>>>,
}

impl Server {
    pub async fn accept(&mut self) -> Option<web_transport::Session> {
        loop {
            tokio::select! {
                res = self.quic.accept() => {
                    let conn = res?;
                    self.accept.push(Self::accept_session(conn).boxed());
                }
                res = self.accept.next(), if !self.accept.is_empty() => {
                    match res.unwrap() {
                        Ok(session) => return Some(session),
                        Err(err) => log::warn!("failed to accept QUIC connection: {}", err),
                    }
                }
            }
        }
    }

    async fn accept_session(conn: quinn::Incoming) -> anyhow::Result<web_transport::Session> {
        let mut conn = conn.accept()?;

        let handshake = conn
            .handshake_data()
            .await?
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .unwrap();

        let alpn = handshake.protocol.context("missing ALPN")?;
        let alpn = String::from_utf8_lossy(&alpn);
        let server_name = handshake.server_name.unwrap_or_default();

        log::debug!(
            "received QUIC handshake: ip={} alpn={} server={}",
            conn.remote_address(),
            alpn,
            server_name,
        );

        // Wait for the QUIC connection to be established.
        let conn = conn.await.context("failed to establish QUIC connection")?;

        log::debug!(
            "established QUIC connection: id={} ip={} alpn={} server={}",
            conn.stable_id(),
            conn.remote_address(),
            alpn,
            server_name,
        );

        // FIX: Use proper WebTransport API
        let session: web_transport::Session = if alpn == web_transport_quinn::ALPN {
            let request = web_transport_quinn::Request::accept(conn) // FIX: Use Request::accept
                .await
                .context("failed to receive WebTransport request")?;
            let sess = request
                .ok()
                .await
                .context("failed to respond to WebTransport request")?;
            sess.into()
        } else if alpn.as_bytes() == moq_transport::setup::ALPN {
            // For MoQ, create a dummy URL since we don't have the original URL here
            let dummy_url = Url::parse("moqt://localhost").unwrap();
            web_transport_quinn::Session::raw(conn, dummy_url).into()
        } else {
            anyhow::bail!("unsupported ALPN: {}", alpn)
        };

        Ok(session.into())
    }

    // ÚJ: accept, ami visszaadja a Session-t és a QuinnStatsProvider-t is
    pub async fn accept_with_stats(
        &mut self,
    ) -> anyhow::Result<(web_transport::Session, Option<Arc<QuinnStatsProvider>>)> {
        let incoming = self.quic.accept().await.context("accept failed")?;
        Self::accept_session_with_stats(incoming).await
    }

    async fn accept_session_with_stats(
        conn: quinn::Incoming,
    ) -> anyhow::Result<(web_transport::Session, Option<Arc<QuinnStatsProvider>>)> {
        let mut conn = conn.accept()?;

        let handshake = conn
            .handshake_data()
            .await?
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .unwrap();

        let alpn = handshake.protocol.context("missing ALPN")?;
        let alpn = String::from_utf8_lossy(&alpn);
        let server_name = handshake.server_name.unwrap_or_default();

        log::debug!(
            "received QUIC handshake: ip={} alpn={} server={}",
            conn.remote_address(),
            alpn,
            server_name,
        );

        // QUIC kapcsolat létrehozása
        let conn = conn.await.context("failed to establish QUIC connection")?;
        log::debug!(
            "established QUIC connection: id={} ip={} alpn={} server={}",
            conn.stable_id(),
            conn.remote_address(),
            alpn,
            server_name,
        );

        // Provider a Quinn Connection-ből
        let provider = Some(Arc::new(QuinnStatsProvider::new(conn.clone())));

        // WebTransport / MoQ session
        // FIX: Use proper WebTransport API
        let session: web_transport::Session = if alpn == web_transport_quinn::ALPN {
            let request = web_transport_quinn::Request::accept(conn) // FIX: Use Request::accept
                .await
                .context("failed to receive WebTransport request")?;
            let sess = request
                .ok()
                .await
                .context("failed to respond to WebTransport request")?;
            sess.into()
        } else if alpn.as_bytes() == moq_transport::setup::ALPN {
            // For MoQ, create a dummy URL since we don't have the original URL here
            let dummy_url = Url::parse("moqt://localhost").unwrap();
            web_transport_quinn::Session::raw(conn, dummy_url).into()
        } else {
            anyhow::bail!("unsupported ALPN: {}", alpn)
        };

        Ok((session.into(), provider))
    }

    pub fn local_addr(&self) -> anyhow::Result<net::SocketAddr> {
        self.quic
            .local_addr()
            .context("failed to get local address")
    }
}

#[derive(Clone)]
pub struct Client {
    quic: quinn::Endpoint,
    config: rustls::ClientConfig,
    transport: Arc<quinn::TransportConfig>,
}

impl Client {
    /// Connect and return both the webtransport session and an optional QuinnStatsProvider.
    pub async fn connect_with_stats(&self, url: &Url) -> anyhow::Result<(web_transport::Session, Option<Arc<QuinnStatsProvider>>)> {

        let mut config = self.config.clone();

        // TODO support connecting to both ALPNs at the same time
        config.alpn_protocols = vec![match url.scheme() {
            "https" => web_transport_quinn::ALPN.as_bytes().to_vec(),
            // &[u8] konstansnál NEM kell as_bytes()
            "moqt" => moq_transport::setup::ALPN.to_vec(),
            _ => anyhow::bail!("url scheme must be 'https' or 'moqt'"),
        }];

        config.key_log = Arc::new(rustls::KeyLogFile::new());

        let config: quinn::crypto::rustls::QuicClientConfig = config.try_into()?;
        let mut config = quinn::ClientConfig::new(Arc::new(config));
        config.transport_config(self.transport.clone());


        let host = url.host().context("invalid DNS name")?.to_string();
        let port = url.port().unwrap_or(443);


        let addr = tokio::net::lookup_host((host.clone(), port))
            .await
            .context("failed DNS lookup")?
            .next()
            .context("no DNS entries")?;

        // use your existing connection setup (adjust variable names as needed)
        let connection = self.quic.connect_with(config, addr, &host)?.await?;

        // create provider from the connection handle (clone if allowed)
        let provider = Some(Arc::new(QuinnStatsProvider::new(connection.clone())));

        // create webtransport session from the connection (adjust to your existing API)
        let session = match url.scheme() {
            "https" => web_transport_quinn::Session::connect(connection, url.clone()).await?,
            "moqt" => web_transport_quinn::Session::raw(connection, url.clone()),
            _ => unreachable!(),
        };

        // Return the transport Session and the provider separately
        Ok((session.into(), provider))
    }

    /// Backwards-compatible connect() that returns only the webtransport::Session
    pub async fn connect(&self, url: &Url) -> anyhow::Result<web_transport::Session> {
        let (session, _provider) = self.connect_with_stats(url).await?;
        Ok(session)
    }
}
