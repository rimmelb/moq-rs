use std::{net, sync::Arc};

use axum::{extract::{State, Query}, http::Method, response::IntoResponse, routing::get, Router};
use hyper_serve::tls_rustls::RustlsAcceptor;
use tower_http::cors::{Any, CorsLayer};
use serde::Deserialize;
use moq_transport::session::SharedState;


#[derive(Deserialize)]
struct GoawayParams {
    url: String,
    timeout: u64,
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
        let relay_stopping_state = config.relay_stopping_state.clone();

        let app = Router::new()
            .route("/fingerprint", get(serve_fingerprint))
            .route(
                "/goaway",
                axum::routing::post({
                    move |Query(params): Query<GoawayParams>| {
                        let shared_state = shared_state.clone();
                        let _relay_stopping_state = relay_stopping_state.clone();
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
