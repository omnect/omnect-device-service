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
pub async fn reboot(reason: &str, extra_info: &str) -> Result<()> {
    use crate::reboot_reason;
    use anyhow::Context;
    use log::{debug, error};
    use std::process::Command;

    info!("systemd::reboot");

    reboot_reason::write_reboot_reason(reason, extra_info).context(format!(
        "reboot: failed to write reason '{reason}' with info '{extra_info}'"
    ))?;

    //journalctl seems not to have a dbus api
    match Command::new("sudo").args(["journalctl", "--sync"]).status() {
        Ok(status) if !status.success() => error!("reboot: failed to execute 'journalctl --sync'"),
        Err(e) => error!("reboot: failed to execute 'journalctl --sync' with: {e:#}"),
        _ => debug!("reboot: succeeded to execute 'journalctl --sync'"),
    }

    // Spawn reboot in background with a small delay to allow this function to return
    // and the service to respond to the caller before the system shuts down
    let delay_ms = std::env::var("REBOOT_DELAY_MS")
        .unwrap_or("100".to_string())
        .parse::<u64>()
        .unwrap_or(100);

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        debug!("triggering reboot.target");
        if let Err(e) = unit::unit_action(
            "reboot.target",
            unit::UnitAction::Start,
            unit::Mode::Replace,
        )
        .await
        {
            error!("failed to start reboot.target: {e:#}");
        }
    });

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
pub async fn reboot(_reason: &str, _extra_info: &str) -> Result<()> {
    Ok(())
}
