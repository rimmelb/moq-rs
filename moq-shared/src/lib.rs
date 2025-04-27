use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use url::Url;

#[derive(Clone)]
pub struct SharedState {
    state: Arc<Mutex<bool>>,
    url: Arc<Mutex<Option<Url>>>,
    elapsed_time: Arc<Mutex<Option<u64>>>,
    notifier: Arc<Notify>,  // Új mező a változás jelzésére
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            url: Arc::new(Mutex::new(None)), // Inicializálás
            state: Arc::new(Mutex::new(false)),
            elapsed_time: Arc::new(Mutex::new(None)),
            notifier: Arc::new(Notify::new()), // Inicializálás
        }
    }

    pub fn update_with_url(&self, url: Url) {
        {
            let mut stored_url = self.url.lock().unwrap();
            *stored_url = Some(url);
        }
        self.update();
    }

    pub fn update_with_int(&self, new_value: u64) {
        {
            let mut stored_value = self.elapsed_time.lock().unwrap();
            *stored_value = Some(new_value);
        }
        self.update();
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

    pub fn get_url(&self) -> Option<Url> {
        let stored_url = self.url.lock().unwrap();
        stored_url.clone()
    }

    pub fn get_value(&self) -> Option<u64> {
        let stored_value = self.elapsed_time.lock().unwrap();
        *stored_value
    }

    pub async fn wait_for_change(&self) {
        self.notifier.notified().await;
    }
}


impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}
