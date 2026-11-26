use anyhow::Result;
#[derive(Copy, Clone, Debug)]
pub enum Mode {
    Replace,
    Fail,
}

impl Mode {
    #[cfg(not(feature = "mock"))]
    fn as_str(self) -> &'static str {
        match self {
            Mode::Replace => "replace",
            Mode::Fail => "fail",
        }
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
async fn system_manager() -> Result<super::SystemdManagerProxy<'static>> {
    use anyhow::Context;

    let connection = crate::systemd::system_connection().await?;
    super::SystemdManagerProxy::new(&connection)
        .await
        .context("failed to create systemd manager proxy")
}

#[cfg(not(feature = "mock"))]
pub async fn unit_action(unit: &str, unit_action: UnitAction, mode: Mode) -> Result<()> {
    use anyhow::Context;
    use log::debug;
    use tokio_stream::StreamExt;

    debug!("unit_action: {unit} {unit_action:?} {mode:?}");

    let manager = system_manager().await?;
    let job_removed_stream = manager
        .receive_job_removed()
        .await
        .context("failed to create job_removed_stream")?;

    let action = |&unit_action| async move {
        match unit_action {
            UnitAction::Reload => manager.reload_unit(unit, mode.as_str()).await,
            UnitAction::Restart => manager.restart_unit(unit, mode.as_str()).await,
            UnitAction::Start => manager.start_unit(unit, mode.as_str()).await,
            UnitAction::Stop => manager.stop_unit(unit, mode.as_str()).await,
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

/// Names of currently-active units matching the given systemd instance/glob
/// patterns (e.g. `"foo@*.service"`), sorted.
#[cfg(not(feature = "mock"))]
pub async fn active_units_by_patterns(patterns: &[&str]) -> Result<Vec<String>> {
    use anyhow::Context;

    const ACTIVE_STATE_ACTIVE: &str = "active";

    let manager = system_manager().await?;
    let mut names: Vec<String> = manager
        .list_units_by_patterns(&[], patterns)
        .await
        .context("active_units_by_patterns: list_units_by_patterns failed")?
        .into_iter()
        .filter(|unit| unit.active_state == ACTIVE_STATE_ACTIVE)
        .map(|unit| unit.name)
        .collect();
    names.sort();

    Ok(names)
}

/// Stream of unit names whose systemd job just completed (`JobRemoved`).
///
/// systemd only broadcasts Job signals to subscribed peers, so this subscribes
/// once on the same connection the returned match-rule stream owns.
#[cfg(not(feature = "mock"))]
pub async fn job_removed_units() -> Result<impl futures::Stream<Item = String> + Send> {
    use anyhow::Context;
    use futures::StreamExt;
    use log::warn;
    use std::num::NonZeroUsize;

    const SIGNAL_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

    let manager = system_manager().await?;
    manager
        .subscribe()
        .await
        .context("job_removed_units: subscribe failed")?;

    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.systemd1")
        .interface("org.freedesktop.systemd1.Manager")
        .member("JobRemoved")
        .build()
        .context("job_removed_units: invalid match rule")?;

    let stream = zbus::MessageStream::for_match_rule(
        rule,
        manager.inner().connection(),
        Some(SIGNAL_QUEUE_CAPACITY),
    )
    .await
    .context("job_removed_units: for_match_rule failed")?;

    // JobRemoved is (id: u32, job: objpath, unit: string, result: string).
    Ok(stream.filter_map(|msg| async move {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                warn!("job_removed_units: message stream error: {e:#}");
                return None;
            }
        };
        match msg
            .body()
            .deserialize::<(u32, zbus::OwnedObjectPath, String, String)>()
        {
            Ok((_, _, unit, _)) => Some(unit),
            Err(e) => {
                warn!("job_removed_units: JobRemoved deserialize failed: {e:#}");
                None
            }
        }
    }))
}
