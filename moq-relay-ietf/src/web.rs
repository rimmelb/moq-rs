use std::{net, sync::Arc};

use axum::{
    extract::{Query, State},
    http::Method,
    response::IntoResponse,
    routing::get,
    Router,
};
use hyper_serve::tls_rustls::RustlsAcceptor;
use moq_transport::session::SharedState;
use serde::Deserialize;
use axum::routing::post;
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
// TODO remove this when Chrome adds support for self-signed certificates using WebTransport
pub struct Web {
    app: Router,
    server: hyper_serve::Server<RustlsAcceptor>,
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
        let tls = hyper_serve::tls_rustls::RustlsConfig::from_config(Arc::new(tls));

        // Clone the shared state for use in the `/update` handler.
        let shared_state = config.shared_state.clone();
        let _relay_stopping_state = config.relay_stopping_state.clone(); // silence unused for now

        let app = Router::new()
            .route("/fingerprint", get(serve_fingerprint))
            .route(
                "/goaway",
                axum::routing::post({
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
            // ÚJ: dinamikus rate limit
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
                                m as u64
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
                    let shared_state = config.shared_state.clone();
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

        let server = hyper_serve::bind_rustls(config.bind, tls);

        Self { app, server }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        self.server.serve(self.app.into_make_service()).await?;
        Ok(())
    }
}

async fn serve_fingerprint(State(fingerprint): State<String>) -> impl IntoResponse {
    fingerprint
}
