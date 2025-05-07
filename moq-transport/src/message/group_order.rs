use crate::coding::{Decode, DecodeError, Encode, EncodeError};

/// Group Order
/// https://www.ietf.org/archive/id/draft-ietf-moq-transport-05.html#section-6.4.2-4.6.1
/// https://www.ietf.org/archive/id/draft-ietf-moq-transport-05.html#priorities
#[derive(Clone, Debug, PartialEq)]
pub enum GroupOrder {
    Publisher = 0x0,
    Ascending = 0x1,
    Descending = 0x2,
}

impl Encode for GroupOrder {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        match self {
            Self::Publisher => (0x0_u8).encode(w),
            Self::Ascending => (0x1_u8).encode(w),
            Self::Descending => (0x2_u8).encode(w),
        }
    }
}

impl Decode for GroupOrder {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        match u8::decode(r)? {
            0x0 => Ok(Self::Publisher),
            0x1 => Ok(Self::Ascending),
            0x2 => Ok(Self::Descending),
            _ => Err(DecodeError::InvalidGroupOrder),
        }
    }
}
