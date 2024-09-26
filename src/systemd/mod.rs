pub mod unit;
pub mod wait_online;
pub mod watchdog;

use anyhow::{bail, Result};
use log::{debug, info};
#[cfg(not(feature = "mock"))]
use log::error;
use sd_notify::NotifyState;
#[cfg(not(feature = "mock"))]
use std::process::Command;
use std::{sync::Once, thread, time, time::Duration};
use systemd_zbus::ManagerProxy;
use tokio::time::{timeout_at, Instant};

pub fn sd_notify_ready() {
    static SD_NOTIFY_ONCE: Once = Once::new();
    SD_NOTIFY_ONCE.call_once(|| {
        info!("notify ready=1");
        let _ = sd_notify::notify(false, &[NotifyState::Ready]);
    });
}

#[cfg(not(feature = "mock"))]
pub async fn reboot() -> Result<()> {
    info!("systemd::reboot");
    //journalctl seems not to have a dbus api
    if let Err(e) = Command::new("sudo")
        .arg("journalctl")
        .arg("--sync")
        .status()
    {
        error!("reboot: failed to execute 'journalctl --sync' with: {e}")
    }

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

pub async fn wait_for_system_running(timeout: Duration) -> Result<()> {
    let begin = Instant::now();
    let deadline = begin + timeout;
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
        if begin.elapsed() >= timeout {
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
