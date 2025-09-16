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
            buffer: Default::default(),
            bandwidth_estimator: None,
        }
    }

    pub fn with_bandwidth_estimator(
        stream: web_transport::RecvStream,
        bandwidth_estimator: Arc<Mutex<BandwidthEstimator>>,
    ) -> Self {
        Self {
            stream,
            buffer: Default::default(),
            bandwidth_estimator: Some(bandwidth_estimator),
        }
    }

    pub async fn decode<T: Decode>(&mut self) -> Result<T, SessionError> {
        loop {
            let mut cursor = io::Cursor::new(&self.buffer);

            let required = match T::decode(&mut cursor) {
                Ok(msg) => {
                    let bytes_consumed = cursor.position() as usize;

                    if let Some(ref estimator) = self.bandwidth_estimator {
                        let mut est = estimator.lock().await; // was: try_lock()
                        est.record_bytes(bytes_consumed as u64);
                        est.update();
                    }

                    self.buffer.advance(bytes_consumed);
                    return Ok(msg);
                }
                Err(DecodeError::More(required)) => self.buffer.len() + required,
                Err(err) => return Err(err.into()),
            };

            loop {
                let before_len = self.buffer.len();
                if !self.stream.read_buf(&mut self.buffer).await? {
                    return Err(DecodeError::More(required - self.buffer.len()).into());
                };

                let bytes_read = self.buffer.len() - before_len;
                if bytes_read > 0 {
                    if let Some(ref estimator) = self.bandwidth_estimator {
                        let mut est = estimator.lock().await; // was: try_lock()
                        est.record_bytes(bytes_read as u64);
                        est.update();
                    }
                }

                if self.buffer.len() >= required {
                    break;
                }
            }
        }
    }

    pub async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>, SessionError> {
        if !self.buffer.is_empty() {
            let size = cmp::min(max, self.buffer.len());
            let data = self.buffer.split_to(size).freeze();

            if let Some(ref estimator) = self.bandwidth_estimator {
                let mut est = estimator.lock().await; // was: try_lock()
                est.record_bytes(size as u64);
                est.update();
            }

            return Ok(Some(data));
        }

        let chunk = self.stream.read_chunk(max).await?;

        if let Some(ref chunk_data) = chunk {
            if let Some(ref estimator) = self.bandwidth_estimator {
                let mut est = estimator.lock().await; // was: try_lock()
                est.record_bytes(chunk_data.len() as u64);
                est.update();
            }
        }

        Ok(chunk)
    }

    pub async fn done(&mut self) -> Result<bool, SessionError> {
        if !self.buffer.is_empty() {
            return Ok(false);
        }

        Ok(!self.stream.read_buf(&mut self.buffer).await?)
    }
}
