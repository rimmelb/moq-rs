mod announce;
mod announced;
mod error;
mod publisher;
mod reader;
mod shared;
mod subscribe;
mod subscribed;
mod subscriber;
mod track_status_requested;
mod writer;

use crate::error::SessionError as OfficialError;
pub use announce::*;
pub use announced::*;
pub use error::*;
pub use publisher::*;
pub use shared::SharedState;
pub use subscribe::*;
pub use subscribed::*;
pub use subscriber::*;
pub use track_status_requested::*;

use reader::*;
use writer::*;

use futures::{stream::FuturesUnordered, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::message::Message;
use crate::watch::Queue;
use crate::{message, setup};

#[must_use = "run() must be called"]
pub struct Session {
    webtransport: web_transport::Session,
    sender: Arc<Mutex<Writer>>,
    recver: Reader,
    publisher: Option<Publisher>,
    subscriber: Option<Subscriber>,
    pub outgoing: Queue<Message>,
}

impl Session {
    fn new(
        webtransport: web_transport::Session,
        sender: Writer,
        recver: Reader,
        role: setup::Role,
    ) -> (Session, Option<Publisher>, Option<Subscriber>) {
        let outgoing = Queue::default().split();
        let publisher = role
            .is_publisher()
            .then(|| Publisher::new(outgoing.0.clone(), webtransport.clone()));
        let subscriber = role.is_subscriber().then(|| Subscriber::new(outgoing.0));

        let session = Self {
            webtransport,
            sender: Arc::new(Mutex::new(sender)),
            recver,
            publisher: publisher.clone(),
            subscriber: subscriber.clone(),
            outgoing: outgoing.1,
        };

        (session, publisher, subscriber)
    }

    pub async fn connect(
        session: web_transport::Session,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        Self::connect_role(session, setup::Role::Both).await.map(
            |(session, publisher, subscriber)| (session, publisher.unwrap(), subscriber.unwrap()),
        )
    }

    pub async fn connect_role(
        mut session: web_transport::Session,
        role: setup::Role,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.open_bi().await?;
        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let versions: setup::Versions = [setup::Version::DRAFT_07].into();

        let client = setup::Client {
            role,
            versions: versions.clone(),
            params: Default::default(),
        };

        log::debug!("sending client SETUP: {:?}", client);
        sender.encode(&client).await?;

        let server: setup::Server = recver.decode().await?;
        log::debug!("received server SETUP: {:?}", server);

        let role = match server.role {
            setup::Role::Both => role,
            setup::Role::Publisher => match role {
                setup::Role::Publisher => {
                    return Err(SessionError::RoleIncompatible(server.role, role))
                }
                _ => setup::Role::Subscriber,
            },
            setup::Role::Subscriber => match role {
                setup::Role::Subscriber => {
                    return Err(SessionError::RoleIncompatible(server.role, role))
                }
                _ => setup::Role::Publisher,
            },
        };
        Ok(Session::new(session, sender, recver, role))
    }

    pub async fn accept(
        session: web_transport::Session,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        Self::accept_role(session, setup::Role::Both).await
    }

    pub async fn accept_role(
        mut session: web_transport::Session,
        role: setup::Role,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.accept_bi().await?;
        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let client: setup::Client = recver.decode().await?;
        log::debug!("received client SETUP: {:?}", client);

        if !client.versions.contains(&setup::Version::DRAFT_07) {
            return Err(SessionError::Version(
                client.versions,
                [setup::Version::DRAFT_07].into(),
            ));
        }

        let role = match client.role {
            setup::Role::Both => role,
            setup::Role::Publisher => match role {
                setup::Role::Publisher => {
                    return Err(SessionError::RoleIncompatible(client.role, role))
                }
                _ => setup::Role::Subscriber,
            },
            setup::Role::Subscriber => match role {
                setup::Role::Subscriber => {
                    return Err(SessionError::RoleIncompatible(client.role, role))
                }
                _ => setup::Role::Publisher,
            },
        };

        let server = setup::Server {
            role,
            version: setup::Version::DRAFT_07,
            params: Default::default(),
        };

        log::debug!("sending server SETUP: {:?}", server);
        sender.encode(&server).await?;
        Ok(Session::new(session, sender, recver, role))
    }

    pub async fn run(self, shared_state: SharedState) -> Result<(), SessionError> {
        let sender = self.sender.clone();
        let shared_state = shared_state.clone();
        let sender_2 = sender.clone();

        let run_goaway = Self::execute_goaway(sender_2, shared_state);

        tokio::select! {
        res = Self::run_recv(self.recver, self.publisher, self.subscriber.clone()) => res,
        res = Self::run_send(self.sender, self.outgoing) => res,
        res = Self::run_streams(self.webtransport.clone(), self.subscriber.clone()) => res,
        res = Self::run_datagrams(self.webtransport, self.subscriber) => res,
        res = run_goaway => res,
        }
    }

    async fn execute_goaway(
        sender: Arc<Mutex<Writer>>,
        shared_state: SharedState,
    ) -> Result<(), SessionError> {
        let shared_state_clone = shared_state.clone();
        Self::goaway_message_send(sender, shared_state.clone()).await?;
        Self::raise_goaway_timeout_error(shared_state_clone).await
    }

    pub async fn raise_goaway_timeout_error(shared_state: SharedState) -> Result<(), SessionError> {
        tokio::time::sleep(std::time::Duration::from_secs(
            shared_state.get_value().unwrap_or(10),
        ))
        .await;
        Err(SessionError::GoawayTimeout(OfficialError::GoawayTimeout))
    }

    async fn run_send(
        sender: Arc<Mutex<Writer>>,
        mut outgoing: Queue<message::Message>,
    ) -> Result<(), SessionError> {
        while let Some(msg) = outgoing.pop().await {
            log::debug!("sending message: {:?}", msg);
            let mut sender = sender.lock().await;
            sender.encode(&msg).await?;
        }
        Ok(())
    }

    async fn goaway_message_send(
        sender: Arc<Mutex<Writer>>,
        shared_state: SharedState,
    ) -> Result<(), SessionError> {
        tokio::select! {
            _ = shared_state.wait_for_change() => {
                let msg = message::Message::GoAway(message::GoAway {
                url: shared_state.get_url().unwrap().to_string(),
                });
                let mut sender = sender.lock().await;
                sender.encode(&msg).await?;
            }
        }
        Ok(())
    }

    pub async fn run_recv(
        mut recver: Reader,
        mut publisher: Option<Publisher>,
        mut subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let msg: message::Message = recver.decode().await?;
            log::debug!("received message: {:?}", msg);

            let msg = match TryInto::<message::Publisher>::try_into(msg) {
                Ok(msg) => {
                    subscriber
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            let msg = match TryInto::<message::Subscriber>::try_into(msg) {
                Ok(msg) => {
                    publisher
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            let msg = match TryInto::<message::Relay>::try_into(msg) {
                Ok(msg) => {
                    if let Some(ref mut pub_) = publisher {
                        if let Err(e) = pub_.recv_goaway(msg.clone()).await {
                            log::warn!("Publisher GoAway Error: {:?}", e);
                        }
                    }
                    if let Some(ref mut sub_) = subscriber {
                        if let Err(e) = sub_.recv_goaway(msg.clone()).await {
                            log::warn!("ubscriber GoAway Error {:?}", e);
                        }
                    }
                    continue;
                }
                Err(msg) => msg,
            };
            unimplemented!("unknown message context: {:?}", msg)
        }
    }

    async fn run_streams(
        mut webtransport: web_transport::Session,
        subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                res = webtransport.accept_uni() => {
                    let stream = res?;
                    let subscriber = subscriber.clone().ok_or(SessionError::RoleViolation)?;

                    tasks.push(async move {
                        if let Err(err) = Subscriber::recv_stream(subscriber, stream).await {
                            log::warn!("failed to serve stream: {}", err);
                        };
                    });
                },
                _ = tasks.next(), if !tasks.is_empty() => {} ,
            };
        }
    }

    async fn run_datagrams(
        mut webtransport: web_transport::Session,
        mut subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let datagram = webtransport.recv_datagram().await?;
            subscriber
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_datagram(datagram)?;
        }
    }
}
