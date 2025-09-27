use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, Mutex as TokioMutex};

use crate::coding::Encode;
use crate::util::BandwidthEstimator;

use super::error::SessionError;
use bytes::BytesMut;

// Pacelés paraméterei
const MAX_CHUNK: usize = 16 * 1024; // csak óvatos chunkolás a write-ra

pub struct Writer {
    pub stream: web_transport::SendStream,
    buffer: BytesMut,
    // Sávszél-mérő (tokio::Mutex-ben)
    bandwidth_estimator: Option<Arc<Mutex<BandwidthEstimator>>>,
}

impl Writer {
    // Alap: nincs rate limit
    pub fn new(stream: web_transport::SendStream) -> Self {
        Self {
            stream,
            buffer: Default::default(),
            bandwidth_estimator: None,
        }
    }

    // Csak mérés
    pub fn with_bandwidth_estimator(
        stream: web_transport::SendStream,
        bandwidth_estimator: Arc<Mutex<BandwidthEstimator>>,
    ) -> Self {
        Self {
            stream,
            buffer: Default::default(),
            bandwidth_estimator: Some(bandwidth_estimator),
        }
    }

    // Egyszerű írás: chunkolt write, limiter nélkül
    async fn write_all_simple(&mut self, buf: &[u8]) -> Result<(), SessionError> {
        let mut pos = 0usize;
        while pos < buf.len() {
            let want = (buf.len() - pos).min(MAX_CHUNK);
            match self.stream.write(&buf[pos..pos + want]).await {
                Ok(n) => {
                    pos += n;
                    if let Some(est) = &self.bandwidth_estimator {
                        let mut e = est.lock().await;
                        e.record_bytes(n as u64);
                        let _ = e.update();
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("STOP_SENDING") || msg.contains("RESET_STREAM") {
                        log::debug!("stream write cancelled by peer: {}", msg);
                        // korábban: return Err(SessionError::Write(e));
                        return Err(e.into());
                    }
                    // korábban: return Err(SessionError::Write(e));
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    pub async fn encode<T: Encode>(&mut self, msg: &T) -> Result<(), SessionError> {
        msg.encode(&mut self.buffer)?;
        let data = std::mem::take(&mut self.buffer).freeze();
        self.write_all_simple(&data).await
    }

    // Nyers buffer küldése limiter nélkül
    pub async fn write(&mut self, buf: &[u8]) -> Result<(), SessionError> {
        self.write_all_simple(buf).await
    }
}
