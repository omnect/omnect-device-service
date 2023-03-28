use anyhow::Result;
use futures_util::{join, StreamExt};
use log::{debug, info};
use sd_notify::NotifyState;
use std::sync::Once;
use std::time::Duration;
use std::time::Instant;
use std::{thread, time};
use systemd_zbus::{ManagerProxy, Mode};
use tokio::time::timeout;

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

pub fn reboot() -> Result<()> {
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

pub fn start_unit(unit: &str) -> Result<()> {
    futures_executor::block_on(async {
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| anyhow::anyhow!("systemd_start_unit: can't get system dbus: {:?}", e))?;
        let proxy = ManagerProxy::new(&conn).await?;

        let mut job_removed_stream = proxy.receive_job_removed().await?;
        let (job_removed, job) = join!(
            timeout(Duration::from_secs(60), job_removed_stream.next()),
            proxy.start_unit(unit, Mode::Fail),
        );

        anyhow::ensure!(
            job.is_ok(),
            "systemd_start_unit: can't start \"{unit}\": {:?}",
            job.err()
        );
        let job = job.unwrap().into_inner();

        anyhow::ensure!(
            job_removed.is_ok(),
            "systemd_job_removed_stream: timeout: {:?}",
            job_removed.err()
        );
        let job_removed = job_removed.unwrap();

        anyhow::ensure!(
            job_removed.is_some(),
            "failed to get next item in job removed stream"
        );
        let job_removed = job_removed.unwrap();

        let job_removed_args = job_removed.args()?;
        debug!("job removed: {:?}", job_removed_args);
        if job_removed_args.job() == &job {
            anyhow::ensure!(
                job_removed_args.result == "done",
                "failed to start unit \"{unit}\": {}",
                job_removed_args.result
            );
        } else {
            loop {
                let job_removed = job_removed_stream.next().await.ok_or_else(|| {
                    anyhow::anyhow!("failed to get next item in job removed stream")
                })?;
                let job_removed_args = job_removed.args()?;
                debug!("job removed: {:?}", job_removed_args);
                if job_removed_args.job() == &job {
                    anyhow::ensure!(
                        job_removed_args.result == "done",
                        "failed to start unit \"{unit}\": {}",
                        job_removed_args.result
                    );
                    break;
                }
            }
        }

        Ok(())
    })
}

/* note: doesnt return a bool */
pub fn is_system_running() -> Result<()> {
    let allowed_system_states = vec!["initializing", "starting", "running"];
    futures_executor::block_on(async {
        let manager = ManagerProxy::new(&zbus::Connection::system().await?).await?;
        loop {
            let system_state = manager.system_state().await?;
            debug!("system is {system_state}");
            anyhow::ensure!(
                { allowed_system_states.contains(&system_state.as_str()) },
                "system is {system_state}"
            );
            if "running" == system_state {
                break;
            } else {
                // ToDo https://github.com/omnect/omnect-device-service/pull/39#discussion_r1142147564
                thread::sleep(time::Duration::from_millis(100));
            };
        }
        Ok(())
    })
}
