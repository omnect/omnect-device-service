use anyhow::{Context, Result};
use log::debug;
pub use systemd_zbus::Mode;
use tokio::time::{Duration, Instant};

#[derive(Copy, Clone, Debug)]
pub enum UnitAction {
    Reload,
    Restart,
    Start,
    Stop,
}

pub async fn unit_action_with_timeout(
    unit: &str,
    unit_action: UnitAction,
    mode: Mode,
    timeout: Duration,
) -> Result<()> {
    debug!("unit_action_with_timeout: {unit} {unit_action:?} {mode:?}");

    tokio::time::timeout(timeout, unit_action_impl(unit, unit_action, mode))
        .await
        .context("unit action timed out")?
}

pub async fn unit_action_with_deadline(
    unit: &str,
    unit_action: UnitAction,
    mode: Mode,
    deadline: Instant,
) -> Result<()> {
    debug!("unit_action_with_deadline: {unit} {unit_action:?} {mode:?}");

    tokio::time::timeout_at(deadline, unit_action_impl(unit, unit_action, mode))
        .await
        .context("unit action timed out")?
}

#[cfg(not(feature = "mock"))]
async fn unit_action_impl(unit: &str, unit_action: UnitAction, mode: Mode) -> Result<()> {
    use anyhow::Context;
    use systemd_zbus::ManagerProxy;
    use tokio_stream::StreamExt;

    let connection = zbus::Connection::system()
        .await
        .context("failed to create connection")?;
    let manager: ManagerProxy<'_> = ManagerProxy::new(&connection)
        .await
        .context("failed to create manager")?;
    let job_removed_stream = manager
        .receive_job_removed()
        .await
        .context("failed to create job_removed_stream")?;

    let action = |&unit_action| async move {
        match unit_action {
            UnitAction::Reload => manager.reload_unit(unit, mode).await,
            UnitAction::Restart => manager.restart_unit(unit, mode).await,
            UnitAction::Start => manager.start_unit(unit, mode).await,
            UnitAction::Stop => manager.stop_unit(unit, mode).await,
        }
    };

    let job = action(&unit_action)
        .await
        .context("unit action failed")?
        .into_inner();

    job_removed_stream
        .filter(|job_removed| {
            if let Ok(args) = job_removed.args() {
                return args.job() == &job && args.result == "done";
            };
            false
        })
        .next()
        .await;

    Ok(())
}

#[cfg(feature = "mock")]
async fn unit_action_impl(_unit: &str, _unit_action: UnitAction, _mode: Mode) -> Result<()> {
    Ok(())
}
