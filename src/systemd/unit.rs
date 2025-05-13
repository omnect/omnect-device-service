use anyhow::Result;
pub use systemd_zbus::Mode;

#[derive(Copy, Clone, Debug)]
pub enum UnitAction {
    Reload,
    Restart,
    Start,
    Stop,
}

#[cfg(not(feature = "mock"))]
pub async fn unit_action(unit: &str, unit_action: UnitAction, mode: Mode) -> Result<()> {
    use anyhow::Context;
    use log::debug;
    use systemd_zbus::ManagerProxy;
    use tokio_stream::StreamExt;

    debug!("unit_action: {unit} {unit_action:?} {mode:?}");

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
pub async fn unit_action(_unit: &str, _unit_action: UnitAction, _mode: Mode) -> Result<()> {
    Ok(())
}
