// session.rs
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::session::SessionError;
use moq_transport::session::SharedState;
use crate::{Consumer, Producer};

pub struct Session {
    pub session: moq_transport::session::Session,
    pub producer: Option<Producer>,
    pub consumer: Option<Consumer>,
}

impl Session {
    pub async fn run(self, shared_state: SharedState) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        tasks.push(self.session.run(shared_state.clone()).boxed());

        if let Some(producer) = self.producer {
            tasks.push(producer.run().boxed());
        }

        if let Some(consumer) = self.consumer {
            tasks.push(consumer.run().boxed());
        }
        tasks.select_next_some().await
    }
}
