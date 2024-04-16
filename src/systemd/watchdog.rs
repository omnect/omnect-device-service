use anyhow::{Context, Result};
use log::{info, trace};
use sd_notify::NotifyState;
use std::sync::{Mutex, OnceLock};
use tokio::time::Instant;

static WATCHDOG_MANAGER: OnceLock<Mutex<Option<WatchdogSettings>>> = OnceLock::new();

struct WatchdogSettings {
    micros: u64,
    now: Instant,
}

pub struct WatchdogManager {}

impl WatchdogManager {
    pub fn init() -> Option<u64> {
        let mut settings = WATCHDOG_MANAGER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap();

        if let Some(settings) = settings.as_ref() {
            Some(settings.micros)
        } else {
            let mut micros = u64::MAX;
            if sd_notify::watchdog_enabled(false, &mut micros) {
                info!("watchdog is enabled with interval: {micros}µs");

                *settings = Some(WatchdogSettings {
                    micros,
                    now: Instant::now(),
                });
                Some(micros)
            } else {
                info!("watchdog is disabled");
                None
            }
        }
    }

    pub fn notify() -> Result<()> {
        let mut settings = WATCHDOG_MANAGER.get().unwrap().lock().unwrap();

        if let Some(settings) = settings.as_mut() {
            trace!("notify watchdog=1");
            sd_notify::notify(false, &[NotifyState::Watchdog]).context("failed to notify")?;
            settings.now = Instant::now();
        }

        Ok(())
    }

    pub fn interval(micros: u128) -> Result<Option<u64>> {
        info!("set interval {micros}µs");

        let mut old_micros = None;
        let micros = u32::try_from(micros).context("casting interval to u32 failed")?;
        let mut settings = WATCHDOG_MANAGER.get().unwrap().lock().unwrap();

        // notify new interval
        sd_notify::notify(false, &[NotifyState::WatchdogUsec(micros)])
            .context("failed to set interval")?;

        // better trigger an extra notify after interval was changed
        sd_notify::notify(false, &[NotifyState::Watchdog])
            .context("failed to notify after setting interval")?;

        if let Some(settings) = settings.as_mut() {
            old_micros = Some(settings.micros);
            settings.micros = micros.into();
            settings.now = Instant::now();
        } else {
            *settings = Some(WatchdogSettings {
                micros: micros.into(),
                now: Instant::now(),
            })
        }

        Ok(old_micros)
    }
}
