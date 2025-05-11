pub mod networkd;
pub mod unit;
pub mod watchdog;

use anyhow::{Context, Result};
use log::info;
use sd_notify::NotifyState;
use std::sync::Once;
use systemd_zbus::ManagerProxy;
use tokio_stream::StreamExt;

pub fn sd_notify_ready() {
    static SD_NOTIFY_ONCE: Once = Once::new();
    SD_NOTIFY_ONCE.call_once(|| {
        info!("notify ready=1");
        let _ = sd_notify::notify(false, &[NotifyState::Ready]);
    });
}

#[cfg(not(feature = "mock"))]
pub async fn reboot() -> Result<()> {
    use anyhow::Context;
    use log::{debug, error};
    use std::process::Command;

    info!("systemd::reboot");

    //journalctl seems not to have a dbus api
    match Command::new("sudo").args(["journalctl", "--sync"]).status() {
        Ok(status) if !status.success() => error!("reboot: failed to execute 'journalctl --sync'"),
        Err(e) => error!("reboot: failed to execute 'journalctl --sync' with: {e:#}"),
        _ => debug!("reboot: succeeded to execute 'journalctl --sync'"),
    }

    zbus::Connection::system()
        .await
        .context("reboot: zbus::Connection::system() failed")?
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Reboot",
            &(true),
        )
        .await
        .context("reboot: call_method() failed")?;

    debug!("reboot: succeeded to call systemd reboot");

    Ok(())
}

pub async fn wait_for_system_running() -> Result<()> {
    let connection = zbus::Connection::system()
        .await
        .context("wait_for_system_running: failed to create connection")?;
    // here we use manager which explicitly doesn't cache the system state
    let manager = ManagerProxy::builder(&connection)
        .uncached_properties(&["SystemState"])
        .build()
        .await
        .context("wait_for_system_running: failed to create manager")?;

    if manager.system_state().await? != "running" {
        manager
            .receive_system_state_changed()
            .await
            .filter(|p| p.name() == "running")
            .next()
            .await;
    }

    Ok(())
}

#[cfg(feature = "mock")]
pub async fn reboot() -> Result<()> {
    Ok(())
}
