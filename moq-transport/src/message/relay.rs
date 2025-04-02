use crate::message::{self, Message};
use std::fmt;

macro_rules! relay_msgs {
    {$($name:ident,)*} => {
		#[derive(Clone)]
		pub enum Relay {
			$($name(message::$name)),*
		}

		$(impl From<message::$name> for Relay {
			fn from(msg: message::$name) -> Self {
				Relay::$name(msg)
			}
		})*

		impl From<Relay> for Message {
			fn from(p: Relay) -> Self {
				match p {
					$(Relay::$name(m) => Message::$name(m),)*
				}
			}
		}

		impl TryFrom<Message> for Relay {
			type Error = Message;

			fn try_from(m: Message) -> Result<Self, Self::Error> {
				match m {
					$(Message::$name(m) => Ok(Relay::$name(m)),)*
					_ => Err(m),
				}
			}
		}

		impl fmt::Debug for Relay {
			// Delegate to the message formatter
			fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				match self {
					$(Self::$name(ref m) => m.fmt(f),)*
				}
			}
		}
    }
}

relay_msgs! {
	GoAway,
}
