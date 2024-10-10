use anyhow::{Context, Result};
use log::{info, trace};
use sd_notify::NotifyState;
use std::{
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::time::{Instant, Interval};

static WATCHDOG_MANAGER: OnceLock<Mutex<Option<WatchdogSettings>>> = OnceLock::new();

struct WatchdogSettings {
    timeout: Duration,
    now: Instant,
}

pub struct WatchdogManager {}

impl WatchdogManager {
    pub fn init() -> Option<Interval> {
        let mut settings = WATCHDOG_MANAGER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap();

        if let Some(settings) = settings.as_ref() {
            // return double sample rate
            Some(tokio::time::interval(settings.timeout / 2))
        } else {
            let mut micros = u64::MAX;
            if sd_notify::watchdog_enabled(false, &mut micros) {
                info!("watchdog is enabled with interval: {micros}µs");

                let timeout = Duration::from_micros(micros);

                *settings = Some(WatchdogSettings {
                    timeout,
                    now: Instant::now(),
                });
                // return double sample rate
                Some(tokio::time::interval(timeout / 2))
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

    pub fn interval(timeout: Duration) -> Result<Option<Duration>> {
        info!("set interval {}µs", timeout.as_micros());

        let mut old_timeout = None;
        let mut settings = WATCHDOG_MANAGER.get().unwrap().lock().unwrap();

        // notify new interval
        sd_notify::notify(
            false,
            &[NotifyState::WatchdogUsec(
                u32::try_from(timeout.as_micros()).context("casting interval to u32 failed")?,
            )],
        )
        .context("failed to set interval")?;

        // better trigger an extra notify after interval was changed
        sd_notify::notify(false, &[NotifyState::Watchdog])
            .context("failed to notify after setting interval")?;

        if let Some(settings) = settings.as_mut() {
            old_timeout = Some(settings.timeout);
            settings.timeout = timeout;
            settings.now = Instant::now();
        } else {
            *settings = Some(WatchdogSettings {
                timeout,
                now: Instant::now(),
            })
        }

        Ok(old_timeout)
    }
}
