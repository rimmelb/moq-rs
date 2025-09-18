use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params, Tuple};
use crate::message::FilterType;
use crate::message::GroupOrder;

/// Sent by the subscriber to request all future objects for the given track.
///
/// Objects will use the provided ID instead of the full track name, to save bytes.
#[derive(Clone, Debug)]
pub struct Subscribe {
    /// The subscription ID
    pub id: u64,

    /// Track properties
    pub track_alias: u64, // This alias is useless but part of the spec
    pub track_namespace: Tuple,
    pub track_name: String,

    // Subscriber Priority
    pub subscriber_priority: u8,
    pub group_order: GroupOrder,

    /// Filter type
    pub filter_type: FilterType,

    /// The start/end group/object. (TODO: Make optional)
    pub start: Option<SubscribePair>, // TODO: Make optional
    pub end: Option<SubscribePair>, // TODO: Make optional

    /// Optional parameters
    pub params: Params,

    /// Delivery timeout in milliseconds (for deadline-aware scheduling)
    /// If None, uses relay default SLA
    pub delivery_timeout_ms: Option<u64>,
}

const PARAM_DELIVERY_TIMEOUT: u64 = 100;

impl Decode for Subscribe {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;
        let track_alias = u64::decode(r)?;
        let track_namespace = Tuple::decode(r)?;
        let track_name = String::decode(r)?;
        let subscriber_priority = u8::decode(r)?;
        let group_order = GroupOrder::decode(r)?;
        let filter_type = FilterType::decode(r)?;

        let (start, end) = match filter_type {
            FilterType::AbsoluteStart => (Some(SubscribePair::decode(r)?), None),
            FilterType::AbsoluteRange => (
                Some(SubscribePair::decode(r)?),
                Some(SubscribePair::decode(r)?),
            ),
            _ => (None, None),
        };

        let mut params = Params::decode(r)?;
        let delivery_timeout_ms = params.get::<u64>(PARAM_DELIVERY_TIMEOUT)?;

        let msg = Self {
            id,
            track_alias,
            track_namespace,
            track_name,
            subscriber_priority,
            group_order,
            filter_type,
            start,
            end,
            params,
            delivery_timeout_ms,
        };

        log::info!(
            "decoded Subscribe id={} timeout={:?} filter={:?}",
            msg.id,
            msg.delivery_timeout_ms,
            msg.filter_type
        );
        Ok(msg)
    }
}

impl Encode for Subscribe {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;
        self.track_alias.encode(w)?;
        self.track_namespace.encode(w)?;
        self.track_name.encode(w)?;
        self.subscriber_priority.encode(w)?;
        self.group_order.encode(w)?;
        self.filter_type.encode(w)?;

        match self.filter_type {
            FilterType::AbsoluteStart => {
                let start = self.start.as_ref().ok_or(EncodeError::MissingField)?;
                start.encode(w)?;
            }
            FilterType::AbsoluteRange => {
                let start = self.start.as_ref().ok_or(EncodeError::MissingField)?;
                let end = self.end.as_ref().ok_or(EncodeError::MissingField)?;
                start.encode(w)?;
                end.encode(w)?;
            }
            _ => {
                // LatestGroup / más: ne írjunk start/end-et függetlenül attól, hogy az Option véletlenül Some
            }
        }

        if let Some(timeout) = self.delivery_timeout_ms {
            let mut p = self.params.clone();
            p.set(PARAM_DELIVERY_TIMEOUT, timeout)?;
            p.encode(w)?;
        } else {
            self.params.encode(w)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubscribePair {
    pub group: SubscribeLocation,
    pub object: SubscribeLocation,
}

impl Decode for SubscribePair {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        Ok(Self {
            group: SubscribeLocation::decode(r)?,
            object: SubscribeLocation::decode(r)?,
        })
    }
}

impl Encode for SubscribePair {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.group.encode(w)?;
        self.object.encode(w)?;
        Ok(())
    }
}

/// Signal where the subscription should begin, relative to the current cache.
#[derive(Clone, Debug, PartialEq)]
pub enum SubscribeLocation {
    None,
    Absolute(u64),
    Latest(u64),
    Future(u64),
}

impl Decode for SubscribeLocation {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let kind = u64::decode(r)?;

        match kind {
            0 => Ok(Self::None),
            1 => Ok(Self::Absolute(u64::decode(r)?)),
            2 => Ok(Self::Latest(u64::decode(r)?)),
            3 => Ok(Self::Future(u64::decode(r)?)),
            _ => Err(DecodeError::InvalidSubscribeLocation),
        }
    }
}

impl Encode for SubscribeLocation {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id().encode(w)?;

        match self {
            Self::None => Ok(()),
            Self::Absolute(val) => val.encode(w),
            Self::Latest(val) => val.encode(w),
            Self::Future(val) => val.encode(w),
        }
    }
}

impl SubscribeLocation {
    fn id(&self) -> u64 {
        match self {
            Self::None => 0,
            Self::Absolute(_) => 1,
            Self::Latest(_) => 2,
            Self::Future(_) => 3,
        }
    }
}
