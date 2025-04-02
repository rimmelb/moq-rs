use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Clone)]
pub struct SharedState {
    state: Arc<Mutex<bool>>,
    notifier: Arc<Notify>,  // Új mező a változás jelzésére
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(false)),
            notifier: Arc::new(Notify::new()), // Inicializálás
        }
    }

    pub fn update(&self) {
        let mut state = self.state.lock().unwrap();
        if !*state {
            *state = true;
            self.notifier.notify_waiters(); // Értesítjük a várakozókat
        }
    }

    pub fn get(&self) -> bool {
        let state = self.state.lock().unwrap();
        *state
    }

    pub async fn wait_for_change(&self) {
        self.notifier.notified().await;
    }
}
