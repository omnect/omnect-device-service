use anyhow::Result;
use std::time::Duration;

cfg_if::cfg_if! {
    if #[cfg(not(any(feature = "mock", test)))] {
        use anyhow::{ensure, Context};
        use futures_util::{join, StreamExt};
        use log::debug;
        use systemd_zbus::{ManagerProxy, Mode};
        use tokio::time::{timeout_at, Instant};
    }
}

#[derive(Copy, Clone, Debug)]
pub enum UnitAction {
    Reload,
    Restart,
    Start,
    Stop,
}

#[cfg(not(feature = "mock"))]
pub async fn unit_action(unit: &str, unit_action: UnitAction, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let system = timeout_at(deadline, zbus::Connection::system()).await??;
    let manager = timeout_at(deadline, ManagerProxy::new(&system)).await??;

    let mut job_removed_stream = timeout_at(deadline, manager.receive_job_removed()).await??;

    let action = |&unit_action| async move {
        match unit_action {
            UnitAction::Reload => manager.reload_unit(unit, Mode::Fail).await,
            UnitAction::Restart => manager.restart_unit(unit, Mode::Fail).await,
            UnitAction::Start => manager.start_unit(unit, Mode::Fail).await,
            UnitAction::Stop => manager.stop_unit(unit, Mode::Fail).await,
        }
    };

    let (job_removed, job) = join!(
        timeout_at(deadline, job_removed_stream.next()),
        timeout_at(deadline, action(&unit_action))
    );

    let job = job
        .context(format!("systemd unit {unit_action:?} \"{unit}\" failed"))??
        .into_inner();

    let job_removed = job_removed
        .context("systemd job_removed_stream")?
        .context("failed to get next item in job removed stream")?;

    let job_removed_args = job_removed.args().context("get removed args")?;

    debug!("job removed: {job_removed_args:?}");
    if job_removed_args.job() == &job {
        ensure!(
            job_removed_args.result == "done",
            "systemd (1) unit {unit_action:?} \"{unit}\" isn't done: {}",
            job_removed_args.result
        );
        return Ok(());
    }

    loop {
        let job_removed = timeout_at(deadline, job_removed_stream.next())
            .await?
            .context("no job_removed signal received")?;

        let job_removed_args = job_removed.args()?;
        debug!("job removed: {job_removed_args:?}");
        if job_removed_args.job() == &job {
            ensure!(
                job_removed_args.result == "done",
                "systemd (2) unit {unit_action:?} \"{unit}\" isn't done: {}",
                job_removed_args.result
            );
            return Ok(());
        }
    }
}

#[cfg(feature = "mock")]
pub async fn unit_action(_unit: &str, _unit_action: UnitAction, _timeout: Duration) -> Result<()> {
    Ok(())
}
