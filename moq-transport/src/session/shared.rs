use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use url::Url;

#[derive(Clone, Default)]
pub struct SharedState {
    state: Arc<Mutex<bool>>,
    url: Arc<Mutex<Option<Url>>>,
    elapsed_time: Arc<Mutex<Option<u64>>>,
    notifier: Arc<Notify>,
    // ÚJ: dinamikus küldési limit (bps)
    rate_limit_bps: crate::util::Watch<Option<u64>>,
    // ÚJ: deadline ütemező konfiguráció
    deadline_cfg: Arc<Mutex<Option<crate::session::DeadlineSchedulerConfig>>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(false)),
            url: Arc::new(Mutex::new(None)),
            elapsed_time: Arc::new(Mutex::new(None)),
            notifier: Arc::new(Notify::new()),
            rate_limit_bps: crate::util::Watch::new(None),
            deadline_cfg: Arc::new(Mutex::new(None)),
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

    pub fn update_with_rate_limit_bps(&self, bps: Option<u64>) {
        let mut w = self.rate_limit_bps.lock_mut();
        *w = bps;

        log::info!(
            "SharedState: rate limit updated: {}",
            bps.map(|v| format!("{} bps ({:.2} Mbps)", v, (v as f64) / 1_000_000.0))
                .unwrap_or_else(|| "disabled".into())
        );
    }


    pub fn update_deadline_scheduler(&self, cfg: crate::session::DeadlineSchedulerConfig) {
        {
            let mut g = self.deadline_cfg.lock().unwrap();
            *g = Some(cfg);
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

    pub fn get_rate_limit_bps(&self) -> Option<u64> {
        *self.rate_limit_bps.lock()
    }

    pub fn get_deadline_scheduler(&self) -> Option<crate::session::DeadlineSchedulerConfig> {
        let g = self.deadline_cfg.lock().unwrap();
        g.clone()
    }

    pub async fn wait_for_change(&self) {
        self.notifier.notified().await;
    }
    pub async fn wait_for_rate_limit_change(&self) {
        self.rate_limit_bps.lock().changed().await;
    }
}
