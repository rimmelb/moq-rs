use crate::coding::{Decode, DecodeError, Encode, EncodeError, Tuple};

/// Sent by the subscriber to reject an Announce after ANNOUNCE_OK
#[derive(Clone, Debug)]
pub struct AnnounceCancel {
    // Echo back the namespace that was reset
    pub namespace: Tuple,
    // An error code.
    pub error_code: u64,
    // An optional, human-readable reason.
    pub reason_phrase: String,
}

impl Decode for AnnounceCancel {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let namespace = Tuple::decode(r)?;
        let error_code = u64::decode(r)?;
        let reason_phrase = String::decode(r)?;

        Ok(Self {
            namespace,
            error_code,
            reason_phrase,
        })
    }
}

impl Encode for AnnounceCancel {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.namespace.encode(w)?;
        self.error_code.encode(w)?;
        self.reason_phrase.encode(w)?;

        Ok(())
    }
}
