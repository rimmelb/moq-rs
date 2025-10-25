// use crate::session; // unused
// session.rs
use crate::{Consumer, Producer};
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::session::SessionError;
use moq_transport::session::SharedState;

pub struct Session {
    pub session: moq_transport::session::Session,
    pub producer: Option<Producer>,
    pub consumer: Option<Consumer>,
}

impl Session {
    pub async fn run(self, shared_state: SharedState, delivery_timeout: Option<u64>) -> Result<(), SessionError> {
    let mut tasks = FuturesUnordered::new();

    // előbb mentsd el, ami kell
    let reporter = self.session.media_qos_reporter.clone();

    // ezután move-old a session-t
    tasks.push(self.session.run(shared_state.clone()).boxed());

    if let Some(producer) = self.producer {
        tasks.push(producer.run(delivery_timeout.clone(), shared_state.clone(), reporter.clone()).boxed());
    }

    if let Some(consumer) = self.consumer {
        tasks.push(consumer.run(reporter.clone()).boxed());
    }

    tasks.select_next_some().await;
        while let Some(res) = tasks.next().await {
            match res {
                Ok(()) => {
                }
                Err(e) => {
                    log::warn!("session task failed: {}", e);
                }
            }
        }
        Ok(())
}

}
