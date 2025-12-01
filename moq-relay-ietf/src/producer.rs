use std::sync::Arc;

use futures::{stream::FuturesUnordered, StreamExt};
use moq_transport::{
    serve::{ServeError, TracksReader},
    session::{Publisher, SessionError, Subscribed}, util::MediaQoSReporter,

};

use moq_transport::session::SharedState;

use crate::{Locals, RemotesConsumer};

#[derive(Clone)]
pub struct Producer {
    remote: Publisher,
    locals: Locals,
    remotes: Option<RemotesConsumer>,
}

impl Producer {
    pub fn new(remote: Publisher, locals: Locals, remotes: Option<RemotesConsumer>) -> Self {
        Self {
            remote,
            locals,
            remotes,
        }
    }

    pub async fn announce(&mut self, tracks: TracksReader, reporter: Arc<MediaQoSReporter>) -> Result<(), SessionError> {
        let report = reporter.clone();
        self.remote.announce(tracks, report).await
    }

    pub async fn run(
        mut self,
        delivery_timeout: Option<u64>,
        shared_state: SharedState,
        reporter: Arc<MediaQoSReporter>,
        enable_relay_side_drop: bool,
        enable_link_capacity_information: bool
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        loop {
            tokio::select! {
                Some(subscribe) = self.remote.subscribed() => {
                    let this = self.clone();
                    let shared_state = shared_state.clone();
                    let value = reporter.clone();
                    tasks.push(async move {
                        let info = subscribe.info.clone();
                        let report = value.clone();
                        if let Err(err) = this.clone().serve(subscribe, delivery_timeout, shared_state, report, enable_relay_side_drop, enable_link_capacity_information).await {
                            log::warn!("failed serving subscribe: {:?}, error: {}", info, err)
                        }
                    });
                },
                _ = tasks.next(), if !tasks.is_empty() => {},
                else => return Ok(()),
            }
        }
    }
}

impl Producer {
    async fn serve(self, subscribe: Subscribed, delivery_timeout: Option<u64>, shared_state: SharedState, reporter: Arc<MediaQoSReporter>, enable_relay_side_drop: bool, enable_link_capacity_information: bool) -> Result<(), anyhow::Error> {
        if let Some(mut local) = self.locals.route(&subscribe.info.namespace) {
            if let Some(track) = local.subscribe(&subscribe.info.name) {
                log::info!("serving from local: {:?}", track.info);
                let report = reporter.clone();
                return Ok(subscribe.serve(track, delivery_timeout.clone(), shared_state.clone(), true, report, enable_relay_side_drop, enable_link_capacity_information).await?);
            }
        }
        if let Some(remotes) = &self.remotes {
            if let Some(remote) = remotes.route(&subscribe.info.namespace).await? {
                if let Some(track) =
                    remote.subscribe(subscribe.info.namespace.clone(), subscribe.info.name.clone())?
                {
                    log::info!("serving from remote: {:?} {:?}", remote.info, track.info);
                    let report = reporter.clone();
                    return Ok(subscribe.serve(track.reader, delivery_timeout.clone(), shared_state.clone(), true, report, enable_relay_side_drop, enable_link_capacity_information).await?);
                }
            }
        }
        Err(ServeError::NotFound.into())
    }
}
