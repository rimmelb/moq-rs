use crate::coding::{Decode, DecodeError, Encode, EncodeError, Tuple};
/// Sent by the server to indicate that the client should connect to a different server.
#[derive(Clone, Debug)]
pub struct FixBandwidth {
    pub bandwidth: u64,
}

impl Decode for FixBandwidth {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let bandwidth = u64::decode(r)?;
        Ok(Self { bandwidth })
    }
}

impl Encode for FixBandwidth {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.bandwidth.encode(w)
    }
}
