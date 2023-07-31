use anyhow::{bail, ensure, Context, Result};
use futures_util::{join, StreamExt};
use log::{debug, info, trace};
use sd_notify::NotifyState;
#[cfg(not(feature = "mock"))]
use std::process::Command;
use std::{sync::Once, thread, time, time::Duration};
use systemd_zbus::{ManagerProxy, Mode};
use tokio::time::{timeout_at, Instant};

static SD_NOTIFY_ONCE: Once = Once::new();

pub fn notify_ready() {
    SD_NOTIFY_ONCE.call_once(|| {
        info!("notify ready=1");
        let _ = sd_notify::notify(false, &[NotifyState::Ready]);
    });
}

#[derive(Default)]
pub struct WatchdogHandler {
    usec: u64,
    now: Option<Instant>,
}

impl WatchdogHandler {
    pub fn new() -> Self {
        let mut usec = u64::MAX;
        let mut now = None;

        if sd_notify::watchdog_enabled(false, &mut usec) {
            usec /= 2;
            now = Some(Instant::now());
        }

        info!(
            "watchdog settings: enabled: {} interval: {}µs",
            now.is_some(),
            usec
        );

        WatchdogHandler { usec, now }
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

#[cfg(not(feature = "mock"))]
pub async fn reboot() -> Result<()> {
    info!("systemd::reboot");
    //journalctl seems not to have a dbus api
    let _ = Command::new("sudo")
        .arg("journalctl")
        .arg("--sync")
        .status()
        .context("reboot: failed to execute 'journalctl --sync'")?;

    zbus::Connection::system()
        .await?
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Reboot",
            &(true),
        )
        .await?;
    Ok(())
}

pub async fn start_unit(timeout_secs: u64, unit: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let system = timeout_at(deadline, zbus::Connection::system()).await??;
    let manager = timeout_at(deadline, ManagerProxy::new(&system)).await??;

    let mut job_removed_stream = timeout_at(deadline, manager.receive_job_removed()).await??;

    let (job_removed, job) = join!(
        timeout_at(deadline, job_removed_stream.next()),
        timeout_at(deadline, manager.start_unit(unit, Mode::Fail))
    );

    let job = job
        .with_context(|| "systemd_start_unit: \"{unit}\"")??
        .into_inner();

    let job_removed = job_removed
        .with_context(|| "systemd_job_removed_stream")?
        .with_context(|| "failed to get next item in job removed stream")?;

    let job_removed_args = job_removed.args().with_context(|| "get removed args")?;

    debug!("job removed: {job_removed_args:?}");
    if job_removed_args.job() == &job {
        ensure!(
            job_removed_args.result == "done",
            "failed to start unit \"{unit}\": {}",
            job_removed_args.result
        );
        return Ok(());
    }

    loop {
        let job_removed = timeout_at(deadline, job_removed_stream.next())
            .await?
            .with_context(|| "no job_removed signal received")?;

        let job_removed_args = job_removed.args()?;
        debug!("job removed: {job_removed_args:?}");
        if job_removed_args.job() == &job {
            ensure!(
                job_removed_args.result == "done",
                "failed to start unit \"{unit}\": {}",
                job_removed_args.result
            );
            return Ok(());
        }
    }
}

pub async fn wait_for_system_running(timeout_secs: u64) -> Result<()> {
    let begin = Instant::now();
    let duration = Duration::from_secs(timeout_secs);
    let deadline = begin + duration;
    let system = timeout_at(deadline, zbus::Connection::system()).await??;
    let manager = timeout_at(
        deadline,
        // here we use manager which explicitly doesn't cache the system state
        ManagerProxy::builder(&system)
            .uncached_properties(&["SystemState"])
            .build(),
    )
    .await??;

    loop {
        // timeout_at doesn't return error/timeout if the future returns immediately, so we have to check.
        // e.g. manager.system_state() returns immediately if using a caching ManagerProxy
        if begin.elapsed() >= duration {
            bail!("wait_for_system_running timeout occurred");
        }

        let system_state = timeout_at(deadline, manager.system_state()).await??;

        match system_state.as_str() {
            "running" => {
                debug!("wait_for_system_running: system_state == running");
                return Ok(());
            }
            "initializing" | "starting" => {
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
            }
            _ => bail!("system in error state: \"{system_state}\""),
        }
    }
}

#[cfg(feature = "mock")]
pub async fn reboot() -> Result<()> {
    Ok(())
}
