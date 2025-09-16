use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, Mutex as TokioMutex};

use crate::coding::Encode;
use crate::util::BandwidthEstimator;

use super::error::SessionError;
use bytes::BytesMut;

// Pacelés paraméterei
const PACER_BURST_SECS: f64 = 0.01; // 10 ms burst, jóval kevesebb “tüske”
const MAX_CHUNK: usize = 1200;      // kb. egy tipikus QUIC stream írás szelet

pub struct RateLimiter {
    bps: u64,      // bitek / s
    tokens: f64,   // tokenek (byte-ban)
    last: Instant,
    capacity: f64, // max token (byte) = rate_bytes_per_s * burst_secs
}

impl RateLimiter {
    pub fn new(bps: u64) -> Self {
        let mut rl = Self {
            bps,
            tokens: 0.0,
            last: Instant::now(),
            capacity: 0.0,
        };
        rl.recalc_capacity();
        rl
    }

    fn recalc_capacity(&mut self) {
        let rate_bytes_per_s = (self.bps as f64) / 8.0;
        self.capacity = rate_bytes_per_s * PACER_BURST_SECS.max(0.001); // min 1ms
        if self.tokens > self.capacity {
            self.tokens = self.capacity;
        }
    }

    pub fn set_bps(&mut self, bps: u64) {
        self.bps = bps.max(1);
        self.recalc_capacity();
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;

        let rate_bytes_per_s = (self.bps as f64) / 8.0;
        self.tokens = (self.tokens + elapsed * rate_bytes_per_s).min(self.capacity);
    }

    pub async fn acquire(&mut self, bytes: usize) {
        let need = bytes as f64;
        loop {
            self.refill();
            if self.tokens >= need {
                self.tokens -= need;
                return;
            }
            let rate_bytes_per_s = (self.bps as f64) / 8.0;
            let missing = (need - self.tokens).max(0.0);
            let wait = missing / rate_bytes_per_s; // sec
            let wait = std::time::Duration::from_secs_f64(wait.max(0.0));
            tokio::time::sleep(wait).await;
        }
    }

    // Mennyi írható most rögtön (byte), refill után
    pub fn available_bytes(&mut self) -> usize {
        self.refill();
        self.tokens.floor() as usize
    }

    // Hány byte/ms a jelenlegi limit
    pub fn bytes_per_ms(&self) -> f64 {
        (self.bps as f64) / 8.0 / 1000.0
    }

    // Jelenlegi burst kapacitás byte-ban (legalább 1)
    pub fn capacity_bytes(&self) -> usize {
        self.capacity.max(1.0) as usize
    }

    // Opcionális: azonnali token-ürítés (rate váltáskor)
    pub fn drain(&mut self) {
        self.tokens = 0.0;
    }
}

pub struct Writer {
    pub stream: web_transport::SendStream,
    buffer: BytesMut,
    // Sávszél-mérő (tokio::Mutex-ben)
    bandwidth_estimator: Option<Arc<Mutex<BandwidthEstimator>>>,
    // Megosztott rate limiter
    rate_limiter: Option<Arc<TokioMutex<RateLimiter>>>,
}

impl Writer {
    // Alap: nincs rate limit
    pub fn new(stream: web_transport::SendStream) -> Self {
        Self {
            stream,
            buffer: Default::default(),
            bandwidth_estimator: None,
            rate_limiter: None,
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
            rate_limiter: None,
        }
    }

    // Megosztott limiterrel
    pub fn with_rate_limit(
        stream: web_transport::SendStream,
        bandwidth_estimator: Option<Arc<Mutex<BandwidthEstimator>>>,
        rate_limiter: Option<Arc<TokioMutex<RateLimiter>>>,
    ) -> Self {
        Self {
            stream,
            buffer: Default::default(),
            bandwidth_estimator,
            rate_limiter,
        }
    }

    // Pacelt írás: chunkonként kér tokeneket és ír
    async fn paced_write_all(&mut self, buf: &[u8]) -> Result<(), SessionError> {
        let mut pos = 0usize;

        while pos < buf.len() {
            let mut want = (buf.len() - pos).min(MAX_CHUNK);

            if let Some(ref limiter) = self.rate_limiter {
                let mut rl = limiter.lock().await;
                // korlátozd a szelet méretét a burst kapacitásra és az azonnal elérhető tokenekre
                let cap = rl.capacity_bytes().max(1);
                let avail = rl.available_bytes();
                if avail == 0 {
                    // várj egy kis időt, amíg lesz legalább 1 byte-nyi token
                    let wait_ms = (1.0 / rl.bytes_per_ms()).ceil().max(1.0) as u64;
                    drop(rl);
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                want = want.min(cap).min(avail);
                // token levonása azonnal (nem várunk nagy burstre)
                rl.tokens -= want as f64;
                drop(rl);
            }

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
                        return Ok(());
                    }
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    // Üzenet kódolása és pacelt kiküldése
    pub async fn encode<T: Encode>(&mut self, msg: &T) -> Result<(), SessionError> {
        msg.encode(&mut self.buffer)?;
        let data = std::mem::take(&mut self.buffer).freeze();
        self.paced_write_all(&data).await
    }

    // Nyers buffer pacelt kiküldése
    pub async fn write(&mut self, buf: &[u8]) -> Result<(), SessionError> {
        self.paced_write_all(buf).await
    }

    // ÚJ: limiter beállítása utólag (kontroll csatornához is)
    pub fn set_rate_limiter(&mut self, rl: Option<Arc<TokioMutex<RateLimiter>>>) {
        self.rate_limiter = rl;
    }
}
