use azure_iot_sdk::client::IotError;
use log::{debug, info};
use sd_notify::NotifyState;
use std::sync::Once;
use std::time::Instant;

static SD_NOTIFY_ONCE: Once = Once::new();

pub fn notify_ready() {
    SD_NOTIFY_ONCE.call_once(|| {
        info!("notify ready=1");
        let _ = sd_notify::notify(false, &[NotifyState::Ready]);
    });
}

pub struct WatchdogHandler {
    usec: u64,
    now: Option<Instant>,
}

impl Default for WatchdogHandler {
    fn default() -> Self {
        WatchdogHandler {
            usec: u64::MAX,
            now: None,
        }
    }
}

impl WatchdogHandler {
    pub fn init(&mut self) -> Result<(), IotError> {
        self.usec = u64::MAX;

        if sd_notify::watchdog_enabled(false, &mut self.usec) {
            self.usec = self.usec / 2;
            self.now = Some(Instant::now());
        }

        info!(
            "watchdog settings: enabled: {} interval: {}µs",
            self.now.is_some(),
            self.usec
        );

        Ok(())
    }

    pub fn notify(&mut self) -> Result<(), IotError> {
        if let Some(ref mut now) = self.now {
            if u128::from(self.usec) < now.elapsed().as_micros() {
                debug!("notify watchdog=1");
                let _ = sd_notify::notify(false, &[NotifyState::Watchdog])?;
                *now = Instant::now();
            }
        }

        Ok(())
    }
}
