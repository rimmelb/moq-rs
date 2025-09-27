use std::{cmp, io, sync::Arc};

use bytes::{Buf, Bytes, BytesMut};
use tokio::sync::Mutex;

use crate::coding::{Decode, DecodeError};
use crate::util::BandwidthEstimator;

use super::SessionError;

pub struct Reader {
    stream: web_transport::RecvStream,
    buffer: BytesMut,
    bandwidth_estimator: Option<Arc<Mutex<BandwidthEstimator>>>,
}

impl Reader {
    pub fn new(stream: web_transport::RecvStream) -> Self {
        Self {
            stream,
            buffer: BytesMut::with_capacity(16 * 1024),
            bandwidth_estimator: None,
        }
    }

    pub fn with_bandwidth_estimator(
        stream: web_transport::RecvStream,
        bandwidth_estimator: Arc<Mutex<BandwidthEstimator>>,
    ) -> Self {
        Self {
            stream,
            buffer: BytesMut::with_capacity(16 * 1024),
            bandwidth_estimator: Some(bandwidth_estimator),
        }
    }

    pub async fn decode<T: Decode>(&mut self) -> Result<T, SessionError> {
        loop {
            let mut cursor = io::Cursor::new(&self.buffer);

            let required = match T::decode(&mut cursor) {
                Ok(msg) => {
                    self.buffer.advance(cursor.position() as usize);
                    return Ok(msg);
                }
                Err(DecodeError::More(required)) => self.buffer.len() + required,
                Err(err) => return Err(err.into()),
            };

            // Töltsd a pufferbe, amíg el nem érjük a szükséges méretet vagy EOF
            loop {
                match self.stream.read_buf(&mut self.buffer).await? {
                    Some(n) => {
                        if let Some(est) = &self.bandwidth_estimator {
                            let mut e = est.lock().await;
                            e.record_bytes(n as u64);
                            let _ = e.update();
                        }
                        if self.buffer.len() >= required {
                            break;
                        }
                    }
                    None => {
                        // többet nem kapunk, jelezd, hogy több kellene
                        return Err(DecodeError::More(required - self.buffer.len()).into());
                    }
                }
            }
        }
    }

    pub async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>, SessionError> {
        // Először szolgáljuk ki a belső puffert
        if !self.buffer.is_empty() {
            let size = cmp::min(max, self.buffer.len());
            let data = self.buffer.split_to(size).freeze();
            return Ok(Some(data));
        }

        // Közvetlen olvasás a transzporttól chunk-ban
        let chunk = self.stream.read(max).await?;
        if let Some(bytes) = &chunk {
            if let Some(est) = &self.bandwidth_estimator {
                let mut e = est.lock().await;
                e.record_bytes(bytes.len() as u64);
                let _ = e.update();
            }
        }
        Ok(chunk)
    }

    pub async fn done(&mut self) -> Result<bool, SessionError> {
        if !self.buffer.is_empty() {
            return Ok(false);
        }
        Ok(self.stream.read_buf(&mut self.buffer).await?.is_none())
    }
}
