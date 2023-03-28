use anyhow::Context;
use anyhow::{ensure, Result};
use futures_util::{join, StreamExt};
use log::{debug, info, trace};
use sd_notify::NotifyState;
use std::process::Command;
use std::sync::Once;
use std::time::Duration;
use std::time::Instant;
use std::{thread, time};
use systemd_zbus::{ManagerProxy, Mode};
use tokio::time::timeout;
use zbus::Connection;

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
                trace!("notify watchdog=1");
                sd_notify::notify(false, &[NotifyState::Watchdog])?;
                *now = Instant::now();
            }
        }

        Ok(())
    }
}

pub fn reboot() -> Result<()> {
    info!("systemd::reboot");
    //journalctl seems not to have a dbus api
    let _ = Command::new("sudo")
        .arg("journalctl")
        .arg("--sync")
        .status();

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

pub async fn start_unit(timeout_secs: u64, unit: &str) -> Result<()> {
    let (mut dur, mut begin) = (Duration::from_secs(timeout_secs), Instant::now());
    let manager = timeout(dur, ManagerProxy::new(&zbus::Connection::system().await?)).await?;
    ensure!(manager.is_ok(), "{:?}", manager.err());
    let manager = manager.unwrap();
    (dur, begin) = (
        dur.checked_sub(Instant::now() - begin)
            .with_context(|| "timeout starting unit \"{unit}\"")?,
        Instant::now(),
    );

    let job_removed_stream = timeout(dur, manager.receive_job_removed()).await?;
    ensure!(job_removed_stream.is_ok(), "{:?}", job_removed_stream.err());
    let mut job_removed_stream = job_removed_stream.unwrap();
    (dur, begin) = (
        dur.checked_sub(Instant::now() - begin)
            .with_context(|| "timeout starting unit \"{unit}\"")?,
        Instant::now(),
    );

    let (job_removed, job) = join!(
        timeout(dur, job_removed_stream.next()),
        timeout(dur, manager.start_unit(unit, Mode::Fail))
    );

    anyhow::ensure!(
        job.is_ok(),
        "systemd_start_unit: timeout \"{unit}\": {:?}",
        job.err()
    );
    let job = job.unwrap();

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
        Ok(())
    } else {
        (dur, begin) = (
            dur.checked_sub(Instant::now() - begin)
                .with_context(|| "timeout starting unit \"{unit}\"")?,
            Instant::now(),
        );
        loop {
            let job_removed = timeout(dur, job_removed_stream.next()).await?;
            ensure!(job_removed.is_some(), "no job_removed signal received");
            let job_removed = job_removed.unwrap();

            let job_removed_args = job_removed.args()?;
            debug!("job removed: {:?}", job_removed_args);
            if job_removed_args.job() == &job {
                anyhow::ensure!(
                    job_removed_args.result == "done",
                    "failed to start unit \"{unit}\": {}",
                    job_removed_args.result
                );
                return Ok(());
            }
            (dur, begin) = (
                dur.checked_sub(Instant::now() - begin)
                    .with_context(|| "timeout starting unit \"{unit}\"")?,
                Instant::now(),
            );
        }
    }
}

/* note: doesnt return a bool */
pub async fn is_system_running(timeout_secs: u64) -> Result<()> {
    let allowed_system_states = vec!["initializing", "starting", "running"];
    let (mut dur, mut begin) = (Duration::from_secs(timeout_secs), Instant::now());
    let manager = timeout(
        dur,
        // here we use manager which explicitly doesn't cache the system state
        ManagerProxy::builder(&Connection::system().await?)
            .uncached_properties(&["SystemState"])
            .build(),
    )
    .await?;
    ensure!(manager.is_ok(), "{:?}", manager.err());
    let manager = manager.unwrap();
    (dur, begin) = (
        dur.checked_sub(Instant::now() - begin)
            .with_context(|| "timeout getting system state \"running\"")?,
        Instant::now(),
    );
    loop {
        let system_state = timeout(dur, manager.system_state()).await?;
        ensure!(
            system_state.is_ok(),
            "system state error: {:?}",
            system_state.err()
        );
        let system_state = system_state.unwrap();
        anyhow::ensure!(
            { allowed_system_states.contains(&system_state.as_str()) },
            "system in error state: \"{system_state}\""
        );
        if "running" == system_state {
            debug!("running");
            break;
        } else {
            /*
             * ToDo https://github.com/omnect/omnect-device-service/pull/39#discussion_r1142147564
             * This is tricky because you have to receive system state signals, which blocks if you
             * are already in state "running". if you receive the system state on condition after
             * you got state "starting" by polling, there would by a race which can result in a
             * deadlock of receiving the state signal again.
             * So a solution would be to poll, start signal receiving, poll again and stop
             * possibly stop receiving.
             */

            thread::sleep(time::Duration::from_millis(100));
            trace!("{dur:?}:{begin:?}");
            (dur, begin) = (
                dur.checked_sub(Instant::now() - begin)
                    .with_context(|| "timeout getting system state \"running\"")?,
                Instant::now(),
            );
        }
    }
    Ok(())
}
