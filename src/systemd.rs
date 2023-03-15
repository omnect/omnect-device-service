use anyhow::Result;
use log::{debug, info};
use sd_notify::NotifyState;
use std::sync::Once;
use std::time::Instant;
use systemd_zbus::{ManagerProxyBlocking, Mode};

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
    pub fn init(&mut self) -> Result<()> {
        self.usec = u64::MAX;

        if sd_notify::watchdog_enabled(false, &mut self.usec) {
            self.usec /= 2;
            self.now = Some(Instant::now());
        }

        info!(
            "watchdog settings: enabled: {} interval: {}µs",
            self.now.is_some(),
            self.usec
        );

        Ok(())
    }

    pub fn notify(&mut self) -> Result<()> {
        if let Some(ref mut now) = self.now {
            if u128::from(self.usec) < now.elapsed().as_micros() {
                debug!("notify watchdog=1");
                sd_notify::notify(false, &[NotifyState::Watchdog])?;
                *now = Instant::now();
            }
        }

        Ok(())
    }
}

pub fn system_reboot() -> Result<()> {
    zbus::blocking::Connection::system()
        .map_err(|e| anyhow::anyhow!("system_reboot: can't get system dbus: {:?}", e))?
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Reboot",
            &(true),
        )
        .map_err(|e| anyhow::anyhow!("dbus reboot: {:?}", e))?;

    Ok(())
}

pub fn systemd_start_unit(unit: &str) -> Result<()> {
    let conn = zbus::blocking::Connection::system()
        .map_err(|e| anyhow::anyhow!("systemd_start_unit: can't get system dbus: {:?}", e))?;
    let proxy = ManagerProxyBlocking::new(&conn)?;
    let res = proxy
        .start_unit(unit, Mode::Fail)
        .map_err(|e| anyhow::anyhow!("systemd_start_unit: can't start \"{unit}\": {:?}", e))?;
    debug!("systemd start unit object: {:?}", res);

    //todo receive job removed signal on res
    Ok(())
}
