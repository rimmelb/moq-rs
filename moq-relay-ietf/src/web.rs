use std::{net, sync::Arc};

use axum::{
    extract::{Query, State},
    http::Method,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use moq_transport::session::SharedState;
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

#[derive(Deserialize)]
struct GoawayParams {
    url: String,
    timeout: u64,
}

#[derive(Deserialize)]
struct RateLimitParams {
    bps: Option<u64>,
    mbps: Option<f64>,
}

#[derive(Deserialize)]
struct DeadlineParams {
    enable: Option<bool>,
    mode: Option<String>,     // "edf" | "lstf"
    guard_ms: Option<u64>,    // default pl. 10
    beta: Option<f64>,        // default pl. 0.9
}

pub struct WebConfig {
    pub bind: net::SocketAddr,
    pub tls: moq_native_ietf::tls::Config,
    pub shared_state: SharedState,
    pub relay_stopping_state: SharedState,
}

// Run a HTTP server using Axum
pub struct Web {
    app: Router,
    bind: net::SocketAddr,
    tls: Arc<rustls::ServerConfig>,
}

impl Web {
    pub fn new(config: WebConfig) -> Self {
        // Get the first certificate's fingerprint.
        let fingerprint = config
            .tls
            .fingerprints
            .first()
            .expect("missing certificate")
            .clone();

        let mut tls = config.tls.server.expect("missing server configuration");
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let tls = Arc::new(tls);

        // Clone the shared state for use in handlers
        let shared_state = config.shared_state.clone();

        let app = Router::new()
            .route("/fingerprint", get(serve_fingerprint))
            .route(
                "/goaway",
                post({
                    let shared_state = shared_state.clone();
                    move |Query(params): Query<GoawayParams>| {
                        let shared_state = shared_state.clone();
                        async move {
                            let mut response = String::new();
                            match url::Url::parse(&params.url) {
                                Ok(parsed_url) => {
                                    shared_state.update_with_url(parsed_url);
                                    response.push_str("URL updated. ");
                                }
                                Err(err) => {
                                    response.push_str(&format!("Invalid URL parameter: {}. ", err));
                                }
                            }
                            shared_state.update_with_int(params.timeout);
                            response.push_str("Integer value updated.");
                            response
                        }
                    }
                }),
            )
            .route(
                "/rate_limit",
                post({
                    let shared_state = shared_state.clone();
                    move |Query(params): Query<RateLimitParams>| {
                        let shared_state = shared_state.clone();
                        async move {
                            let bps = if let Some(b) = params.bps {
                                b
                            } else if let Some(m) = params.mbps {
                                (m * 1_000_000.0) as u64
                            } else {
                                return "Missing 'bps' or 'mbps'".into_response();
                            };
                            shared_state.update_with_rate_limit_bps(Some(bps));
                            format!("Rate limit updated: {} bps ({:.2} Mbps)", bps, (bps as f64)/1_000_000.0).into_response()
                        }
                    }
                }),
            )
            .route(
                "/deadline_scheduler",
                post({
                    let shared_state = shared_state.clone();
                    move |Query(params): Query<DeadlineParams>| {
                        let shared_state = shared_state.clone();
                        async move {
                            let enabled = params.enable.unwrap_or(true);
                            let mode = match params.mode.as_deref() {
                                Some("edf") => moq_transport::session::DeadlineMode::Edf,
                                _ => moq_transport::session::DeadlineMode::Lstf,
                            };
                            let cfg = moq_transport::session::DeadlineSchedulerConfig {
                                enabled,
                                mode,
                                guard_ms: params.guard_ms.unwrap_or(10),
                                beta: params.beta.unwrap_or(0.9),
                            };
                            shared_state.update_deadline_scheduler(cfg.clone());
                            format!("Deadline scheduler: enabled={} mode={:?} guard={}ms beta={:.2}",
                                enabled, mode, cfg.guard_ms, cfg.beta).into_response()
                        }
                    }
                }),
            )
            .layer(
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods([Method::GET, Method::POST]),
            )
            .with_state(fingerprint);

        Self { app, bind: config.bind, tls }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_config(self.tls);

        axum_server::bind_rustls(self.bind, tls_config)
            .serve(self.app.into_make_service())
            .await?;

        Ok(())
    }
}

async fn serve_fingerprint(State(fingerprint): State<String>) -> impl IntoResponse {
    fingerprint
}
