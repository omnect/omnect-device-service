pub mod networkd;
pub mod unit;
pub mod watchdog;

use anyhow::{Context, Result};
use log::{error, info, warn};
use sd_notify::NotifyState;
use serde::Deserialize;
use std::{
    future::Future,
    mem::Discriminant,
    sync::Once,
    time::{Duration, Instant},
};
use zbus::{OwnedObjectPath, Type, proxy::CacheProperties};

const SYSTEM_STATE_RUNNING: &str = "running";
const SYSTEM_STATE_DEGRADED: &str = "degraded";
const ACTIVE_STATE_ACTIVATING: &str = "activating";
const ACTIVE_STATE_FAILED: &str = "failed";
const SUB_STATE_AUTO_RESTART: &str = "auto-restart";
// the same pending restart with the restart job already queued
const SUB_STATE_AUTO_RESTART_QUEUED: &str = "auto-restart-queued";
const SYSTEM_HEALTHY_POLL_INTERVAL: Duration = Duration::from_secs(2);
// a single poll can land in a restart window in either direction, so a healthy
// and an unhealthy verdict both need this many observations
const HEALTH_CONFIRMATION_POLLS: u32 = 3;
const CRASH_LOOP_RESTART_THRESHOLD_ENV: &str = "CRASH_LOOP_RESTART_THRESHOLD";
const CRASH_LOOP_RESTART_THRESHOLD_DEFAULT: u32 = 3;
// the unit list ends up in extra_info, which is written to a fixed size pmsg
// record shared by several reboot reasons
const MAX_REPORTED_UNITS: usize = 10;

pub fn sd_notify_ready() {
    static SD_NOTIFY_ONCE: Once = Once::new();
    SD_NOTIFY_ONCE.call_once(|| {
        info!("notify ready=1");
        let _ = sd_notify::notify(&[NotifyState::Ready]);
    });
}

async fn system_connection() -> Result<zbus::Connection> {
    zbus::Connection::system()
        .await
        .context("failed to connect to system bus")
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

// ListUnits as returned by systemd. The states are strings, so a state that a
// newer systemd adds cannot fail the whole reply and break the update validation.
// All fields have to be there to match the signature, only some are read.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Type)]
struct ListedUnit {
    name: String,
    description: String,
    load_state: String,
    active_state: String,
    sub_state: String,
    followed_unit: String,
    path: OwnedObjectPath,
    queued_job: u32,
    job_type: String,
    job_path: OwnedObjectPath,
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    #[zbus(property)]
    fn system_state(&self) -> zbus::Result<String>;

    fn list_units(&self) -> zbus::Result<Vec<ListedUnit>>;

    fn list_units_by_patterns(
        &self,
        states: &[&str],
        patterns: &[&str],
    ) -> zbus::Result<Vec<ListedUnit>>;

    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    fn reload_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    fn subscribe(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: zbus::ObjectPath<'_>,
        unit: &str,
        result: &str,
    ) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Service",
    default_service = "org.freedesktop.systemd1"
)]
trait SystemdService {
    #[zbus(property)]
    fn n_restarts(&self) -> zbus::Result<u32>;
}

pub async fn wait_for_system_healthy(deadline: Duration) -> Result<()> {
    let connection = system_connection().await?;
    // we poll SystemState explicitly; the property cache must not hide changes
    let manager = SystemdManagerProxy::builder(&connection)
        .uncached_properties(&["SystemState"])
        .build()
        .await
        .context("wait_for_system_healthy: failed to create manager")?;

    watch_system_health(
        SYSTEM_HEALTHY_POLL_INTERVAL,
        deadline,
        crash_loop_restart_threshold(),
        || poll_system_health(&connection, &manager),
    )
    .await
}

// the poll is injected so the decision can be tested without a system bus
async fn watch_system_health<F, Fut>(
    poll_interval: Duration,
    deadline: Duration,
    threshold: u32,
    mut poll: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(SystemState, Vec<UnitHealth>)>>,
{
    let start = Instant::now();
    let mut tally = HealthTally::default();
    let mut same_class_polls = 0u32;
    let mut last_class: Option<Discriminant<SystemHealth>> = None;
    let mut last_health: Option<SystemHealth> = None;
    let mut poll_errors = 0u32;

    loop {
        match poll().await {
            Ok((state, units)) => {
                poll_errors = 0;
                let health = rate_system_health(&state, &units, threshold);

                let current = std::mem::discriminant(&health);
                same_class_polls = if last_class == Some(current) {
                    same_class_polls + 1
                } else {
                    1
                };
                last_class = Some(current);
                tally.observe(&health);

                if same_class_polls >= HEALTH_CONFIRMATION_POLLS {
                    match &health {
                        SystemHealth::Healthy => return Ok(()),
                        SystemHealth::Degraded(units) => anyhow::bail!(degraded_extra_info(units)),
                        SystemHealth::CrashLooping(units) => {
                            anyhow::bail!(crash_loop_extra_info(units))
                        }
                        // no final verdict, so these keep polling until the deadline
                        SystemHealth::Restarting(_) | SystemHealth::Starting(_) => {}
                    }
                }

                last_health = Some(health);
            }
            // a bus that stays broken is a failure, but a single failed poll is
            // no observation and must not decide, just like a single unhealthy one
            Err(e) => {
                poll_errors += 1;
                if poll_errors >= HEALTH_CONFIRMATION_POLLS {
                    // the reboot reason takes the outermost message only, so the
                    // cause has to be in it
                    anyhow::bail!("failed to poll system health repeatedly: {e:#}");
                }
                warn!("system health poll failed, retrying: {e:#}");
                same_class_polls = 0;
                last_class = None;
            }
        }

        if start.elapsed() >= deadline {
            return deadline_verdict(&tally, last_health.as_ref());
        }

        tokio::time::sleep(poll_interval).await;
    }
}

// the verdict once the deadline is reached: an unhealthy tally outweighs the
// last observation, so a system that alternates is not decided by whichever
// poll happened to be last
fn deadline_verdict(tally: &HealthTally, last_health: Option<&SystemHealth>) -> Result<()> {
    if let Some(info) = tally.unhealthy_verdict() {
        return Err(anyhow::anyhow!("{info}"));
    }

    // a healthy observation the tally did not turn into a verdict still speaks
    // for the update, whatever state the last poll happened to see
    if tally.healthy_polls > 0 {
        return Ok(());
    }

    match last_health {
        // reaching the crash loop threshold takes threshold x RestartSec, much
        // longer than the confirmation polls, so a pending restart is no proof
        // of a loop and must not roll back the update
        Some(SystemHealth::Restarting(units)) => {
            warn!(
                "deadline reached while units were still restarting: {}",
                report_units(units)
            );
            Ok(())
        }
        Some(SystemHealth::Starting(state)) => Err(anyhow::anyhow!(
            "system not healthy within deadline, last state: {state}"
        )),
        // an unhealthy observation the tally did not confirm is no basis for a
        // rollback
        Some(SystemHealth::Healthy | SystemHealth::Degraded(_) | SystemHealth::CrashLooping(_)) => {
            Ok(())
        }
        // every poll failed
        None => Err(anyhow::anyhow!(
            "no system health observation within deadline"
        )),
    }
}

async fn poll_system_health(
    connection: &zbus::Connection,
    manager: &SystemdManagerProxy<'_>,
) -> Result<(SystemState, Vec<UnitHealth>)> {
    let state = SystemState::parse(
        &manager
            .system_state()
            .await
            .context("poll_system_health: failed to get system state")?,
    );

    let units = if matches!(state, SystemState::Other(_)) {
        vec![]
    } else {
        collect_unit_health(connection, manager).await?
    };

    Ok((state, units))
}

async fn collect_unit_health(
    connection: &zbus::Connection,
    manager: &SystemdManagerProxy<'_>,
) -> Result<Vec<UnitHealth>> {
    let mut units = vec![];

    for unit in manager
        .list_units()
        .await
        .context("collect_unit_health: failed to list units")?
    {
        // a transient per-unit D-Bus error must not abort the validation, so it
        // counts as 0
        let n_restarts = if unit.name.ends_with(".service")
            && in_restart_cycle(&unit.active_state, &unit.sub_state)
        {
            service_n_restarts(connection, &unit).await.unwrap_or(0)
        } else {
            0
        };

        units.push(UnitHealth {
            name: unit.name,
            active_state: unit.active_state,
            sub_state: unit.sub_state,
            n_restarts,
        });
    }

    Ok(units)
}

async fn service_n_restarts(connection: &zbus::Connection, unit: &ListedUnit) -> Result<u32> {
    Ok(SystemdServiceProxy::builder(connection)
        .path(unit.path.clone())
        // a single read per poll: the default cache would subscribe to
        // PropertiesChanged for this unit and be dropped right after
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .n_restarts()
        .await?)
}

#[cfg(feature = "mock")]
pub async fn reboot(_reason: &str, _extra_info: &str) -> Result<()> {
    Ok(())
}

// overridable: how many retries are normal varies per deployment
fn crash_loop_restart_threshold() -> u32 {
    let mut threshold = CRASH_LOOP_RESTART_THRESHOLD_DEFAULT;
    if let Ok(value) = std::env::var(CRASH_LOOP_RESTART_THRESHOLD_ENV) {
        match value.parse::<u32>() {
            // 0 would rate every service in a restart cycle a crash loop
            Ok(0) | Err(_) => error!(
                "ignore invalid crash loop restart threshold {value} and use default {threshold}"
            ),
            Ok(value) => threshold = value,
        };
    }
    threshold
}

#[derive(Debug, Clone, PartialEq)]
struct UnitHealth {
    name: String,
    active_state: String,
    sub_state: String,
    n_restarts: u32,
}

// the systemd system states this check distinguishes
#[derive(Debug, PartialEq)]
enum SystemState {
    Running,
    Degraded,
    Other(String),
}

impl SystemState {
    fn parse(state: &str) -> Self {
        match state {
            SYSTEM_STATE_RUNNING => Self::Running,
            SYSTEM_STATE_DEGRADED => Self::Degraded,
            other => Self::Other(other.to_string()),
        }
    }
}

#[derive(Debug, PartialEq)]
enum SystemHealth {
    Healthy,
    Starting(String),
    Restarting(Vec<String>),
    Degraded(Vec<String>),
    CrashLooping(Vec<String>),
}

#[derive(Default)]
struct CauseTally {
    polls: u32,
    last_info: Option<String>,
}

impl CauseTally {
    fn observe(&mut self, info: String) {
        self.polls += 1;
        self.last_info = Some(info);
    }
}

// all observations of a wait; only healthy and unhealthy ones carry a verdict,
// a starting or restarting system says nothing yet
#[derive(Default)]
struct HealthTally {
    healthy_polls: u32,
    degraded: CauseTally,
    crash_looping: CauseTally,
}

impl HealthTally {
    fn observe(&mut self, health: &SystemHealth) {
        match health {
            SystemHealth::Healthy => self.healthy_polls += 1,
            SystemHealth::Degraded(units) => self.degraded.observe(degraded_extra_info(units)),
            SystemHealth::CrashLooping(units) => {
                self.crash_looping.observe(crash_loop_extra_info(units))
            }
            SystemHealth::Restarting(_) | SystemHealth::Starting(_) => {}
        }
    }

    fn unhealthy_polls(&self) -> u32 {
        self.degraded.polls + self.crash_looping.polls
    }

    // an unhealthy verdict needs as many observations as a confirmed one, only
    // not consecutive; without a single healthy observation there is nothing
    // that speaks for the update, so one unhealthy observation is enough
    fn unhealthy_verdict(&self) -> Option<&str> {
        let unhealthy = self.unhealthy_polls();
        if unhealthy == 0 || (unhealthy < HEALTH_CONFIRMATION_POLLS && self.healthy_polls > 0) {
            return None;
        }

        // the count decides that the system is unhealthy, so the reported cause
        // is the one it counted most often; an equal count reports the crash
        // loop, the more specific of the two
        let cause = if self.crash_looping.polls >= self.degraded.polls {
            &self.crash_looping
        } else {
            &self.degraded
        };
        cause.last_info.as_deref()
    }
}

// systemd parks a unit in the auto-restart sub state between a failed start and
// the next attempt, so only there is a restart pending; a normal start, and a
// unit that recovered or gave up, is not
fn in_restart_cycle(active_state: &str, sub_state: &str) -> bool {
    active_state == ACTIVE_STATE_ACTIVATING
        && matches!(
            sub_state,
            SUB_STATE_AUTO_RESTART | SUB_STATE_AUTO_RESTART_QUEUED
        )
}

fn restart_cycle_units(units: &[UnitHealth], matches: impl Fn(u32) -> bool) -> Vec<String> {
    let mut names: Vec<String> = units
        .iter()
        .filter(|u| in_restart_cycle(&u.active_state, &u.sub_state) && matches(u.n_restarts))
        .map(|u| u.name.clone())
        .collect();
    names.sort();
    names
}

fn crash_looping_units(units: &[UnitHealth], threshold: u32) -> Vec<String> {
    restart_cycle_units(units, |n_restarts| n_restarts >= threshold)
}

fn restarting_units(units: &[UnitHealth], threshold: u32) -> Vec<String> {
    restart_cycle_units(units, |n_restarts| (1..threshold).contains(&n_restarts))
}

fn failed_units(units: &[UnitHealth]) -> Vec<String> {
    let mut failed: Vec<String> = units
        .iter()
        .filter(|u| u.active_state == ACTIVE_STATE_FAILED)
        .map(|u| u.name.clone())
        .collect();
    failed.sort();
    failed
}

fn rate_system_health(state: &SystemState, units: &[UnitHealth], threshold: u32) -> SystemHealth {
    if let SystemState::Other(state) = state {
        return SystemHealth::Starting(state.clone());
    }

    let looping = crash_looping_units(units, threshold);
    if !looping.is_empty() {
        return SystemHealth::CrashLooping(looping);
    }

    if *state == SystemState::Degraded {
        let failed = failed_units(units);
        return if failed.is_empty() {
            // the state was read before the unit list, so the failed unit may
            // already be gone; without a name there is nothing to report
            SystemHealth::Starting(SYSTEM_STATE_DEGRADED.to_string())
        } else {
            SystemHealth::Degraded(failed)
        };
    }

    let restarting = restarting_units(units, threshold);
    if !restarting.is_empty() {
        return SystemHealth::Restarting(restarting);
    }

    SystemHealth::Healthy
}

fn report_units(units: &[String]) -> String {
    if units.len() <= MAX_REPORTED_UNITS {
        return units.join(" ");
    }

    format!(
        "{} (+{} more)",
        units[..MAX_REPORTED_UNITS].join(" "),
        units.len() - MAX_REPORTED_UNITS
    )
}

fn degraded_extra_info(units: &[String]) -> String {
    format!("system degraded, failed units: {}", report_units(units))
}

fn crash_loop_extra_info(units: &[String]) -> String {
    format!("crash-looping units: {}", report_units(units))
}

#[cfg(test)]
mod tests {
    use super::*;

    // the sub state is only read for the restart cycle, so it follows the active
    // state here; a unit in a restart cycle comes from restart_cycle_unit()
    fn unit(name: &str, active_state: &str, n_restarts: u32) -> UnitHealth {
        let sub_state = match active_state {
            "active" => "running",
            ACTIVE_STATE_ACTIVATING => "start",
            other => other,
        };
        UnitHealth {
            name: name.to_string(),
            active_state: active_state.to_string(),
            sub_state: sub_state.to_string(),
            n_restarts,
        }
    }

    fn restart_cycle_unit(name: &str, sub_state: &str, n_restarts: u32) -> UnitHealth {
        UnitHealth {
            name: name.to_string(),
            active_state: ACTIVE_STATE_ACTIVATING.to_string(),
            sub_state: sub_state.to_string(),
            n_restarts,
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    // tests share the process, so a failing assert must not leave the variable
    // behind for whatever test runs next
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn new(key: &'static str, value: &str) -> Self {
            let guard = Self {
                key,
                previous: std::env::var(key).ok(),
            };
            guard.set(value);
            guard
        }

        fn set(&self, value: &str) {
            crate::common::set_env_var(self.key, value);
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => crate::common::set_env_var(self.key, value),
                None => crate::common::remove_env_var(self.key),
            }
        }
    }

    type ScriptedPoll = Result<(SystemState, Vec<UnitHealth>)>;

    const POLL_ERROR: &str = "system bus is gone";

    // drives the decision loop over a scripted poll sequence; an exhausted
    // script is an error, so a test can prove the loop did not stop early
    async fn watch(script: Vec<ScriptedPoll>, deadline: Duration) -> (Result<()>, usize) {
        let mut script = script.into_iter();
        let mut polls = 0usize;
        let result = watch_system_health(
            Duration::ZERO,
            deadline,
            CRASH_LOOP_RESTART_THRESHOLD_DEFAULT,
            || {
                polls += 1;
                std::future::ready(
                    script
                        .next()
                        .unwrap_or_else(|| Err(anyhow::anyhow!("poll script exhausted"))),
                )
            },
        )
        .await;
        (result, polls)
    }

    fn healthy_poll() -> ScriptedPoll {
        Ok((SystemState::Running, vec![unit("a.service", "active", 0)]))
    }

    fn degraded_poll() -> ScriptedPoll {
        Ok((
            SystemState::Degraded,
            vec![unit("a.service", ACTIVE_STATE_FAILED, 0)],
        ))
    }

    fn crash_loop_poll() -> ScriptedPoll {
        Ok((
            SystemState::Running,
            vec![restart_cycle_unit(
                "loop.service",
                SUB_STATE_AUTO_RESTART,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT,
            )],
        ))
    }

    fn restarting_poll() -> ScriptedPoll {
        Ok((
            SystemState::Running,
            vec![restart_cycle_unit(
                "loop.service",
                SUB_STATE_AUTO_RESTART,
                1,
            )],
        ))
    }

    fn failed_poll() -> ScriptedPoll {
        Err(anyhow::anyhow!(POLL_ERROR))
    }

    // the reply is only deserializable if the struct matches what ListUnits
    // returns: a(ssssssouso)
    #[test]
    fn listed_unit_matches_the_list_units_signature() {
        assert_eq!(ListedUnit::SIGNATURE.to_string(), "(ssssssouso)");
    }

    // a state this check does not rate must stay uninteresting instead of
    // failing the poll, which is why the states are read as strings
    #[test]
    fn an_unrated_state_is_healthy() {
        let units = vec![unit("a.service", "refreshing", 5)];
        assert_eq!(
            rate_system_health(
                &SystemState::Running,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Healthy
        );
    }

    #[test]
    fn healthy_when_running_without_crash_loops() {
        let units = vec![unit("a.service", "active", 0)];
        assert_eq!(
            rate_system_health(
                &SystemState::Running,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Healthy
        );
    }

    #[test]
    fn non_final_states_keep_polling() {
        assert_eq!(
            rate_system_health(
                &SystemState::parse("initializing"),
                &[],
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Starting("initializing".to_string())
        );
    }

    #[test]
    fn degraded_lists_failed_units_sorted() {
        let units = vec![
            unit("b.service", ACTIVE_STATE_FAILED, 0),
            unit("a.service", ACTIVE_STATE_FAILED, 0),
        ];
        assert_eq!(
            rate_system_health(
                &SystemState::Degraded,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Degraded(names(&["a.service", "b.service"]))
        );
    }

    #[test]
    fn degraded_lists_failed_non_service_units() {
        let units = vec![unit("data.mount", ACTIVE_STATE_FAILED, 0)];
        assert_eq!(
            rate_system_health(
                &SystemState::Degraded,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Degraded(names(&["data.mount"]))
        );
    }

    // the state is read before the unit list, so the failed unit can be gone by
    // then and there is nothing to name in the reboot reason
    #[test]
    fn degraded_without_failed_units_keeps_polling() {
        let units = vec![unit("a.service", "active", 0)];
        assert_eq!(
            rate_system_health(
                &SystemState::Degraded,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Starting(SYSTEM_STATE_DEGRADED.to_string())
        );
    }

    // the threshold takes threshold x RestartSec to be reached, which is longer
    // than the confirmation polls take, so health must not be confirmed while a
    // restart is pending
    #[test]
    fn restart_below_threshold_is_not_yet_healthy() {
        for n_restarts in 1..CRASH_LOOP_RESTART_THRESHOLD_DEFAULT {
            let units = vec![restart_cycle_unit(
                "loop.service",
                SUB_STATE_AUTO_RESTART,
                n_restarts,
            )];
            assert_eq!(
                rate_system_health(
                    &SystemState::Running,
                    &units,
                    CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
                ),
                SystemHealth::Restarting(names(&["loop.service"])),
                "{n_restarts} restarts should keep the check polling"
            );
        }
    }

    #[test]
    fn system_without_restarts_is_healthy() {
        let units = vec![
            unit("a.service", "active", 0),
            unit("b.timer", "inactive", 0),
        ];
        assert_eq!(
            rate_system_health(
                &SystemState::Running,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Healthy
        );
    }

    #[test]
    fn restart_threshold_is_a_crash_loop() {
        let units = vec![restart_cycle_unit(
            "loop.service",
            SUB_STATE_AUTO_RESTART,
            CRASH_LOOP_RESTART_THRESHOLD_DEFAULT,
        )];
        assert_eq!(
            rate_system_health(
                &SystemState::Running,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::CrashLooping(names(&["loop.service"]))
        );
    }

    // a queued restart is the same pending restart, newer systemd reports it as
    // its own sub state
    #[test]
    fn a_queued_restart_is_a_crash_loop() {
        let units = vec![restart_cycle_unit(
            "loop.service",
            SUB_STATE_AUTO_RESTART_QUEUED,
            CRASH_LOOP_RESTART_THRESHOLD_DEFAULT,
        )];
        assert_eq!(
            rate_system_health(
                &SystemState::Running,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::CrashLooping(names(&["loop.service"]))
        );
    }

    // 'activating' is also a normal start, e.g. by a timer or a dependency; a
    // restart history must not keep the check polling then
    #[test]
    fn a_normal_start_with_restart_history_is_healthy() {
        let units = vec![unit("a.service", ACTIVE_STATE_ACTIVATING, 5)];
        assert_eq!(
            rate_system_health(
                &SystemState::Running,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Healthy
        );
    }

    #[test]
    fn custom_threshold_is_honored() {
        let units = vec![restart_cycle_unit(
            "loop.service",
            SUB_STATE_AUTO_RESTART,
            1,
        )];
        assert_eq!(
            rate_system_health(&SystemState::Running, &units, 1),
            SystemHealth::CrashLooping(names(&["loop.service"]))
        );
    }

    #[test]
    fn zero_threshold_falls_back_to_default() {
        let env = EnvVarGuard::new(CRASH_LOOP_RESTART_THRESHOLD_ENV, "0");
        assert_eq!(
            crash_loop_restart_threshold(),
            CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
        );
        env.set("5");
        assert_eq!(crash_loop_restart_threshold(), 5);
    }

    #[test]
    fn recovered_unit_with_restart_history_is_not_a_crash_loop() {
        let units = vec![unit("a.service", "active", 5)];
        assert_eq!(
            rate_system_health(
                &SystemState::Running,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Healthy
        );
    }

    // a unit that gave up restarting is a failed unit, which the degraded state
    // already reports, so it must not be counted as a loop
    #[test]
    fn unit_that_gave_up_restarting_is_not_a_crash_loop() {
        let units = vec![unit("loop.service", ACTIVE_STATE_FAILED, 5)];
        assert_eq!(
            rate_system_health(
                &SystemState::Degraded,
                &units,
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT
            ),
            SystemHealth::Degraded(names(&["loop.service"]))
        );
    }

    #[test]
    fn unparseable_threshold_falls_back_to_default() {
        let env = EnvVarGuard::new(CRASH_LOOP_RESTART_THRESHOLD_ENV, "");
        for value in ["", "no", "-1"] {
            env.set(value);
            assert_eq!(
                crash_loop_restart_threshold(),
                CRASH_LOOP_RESTART_THRESHOLD_DEFAULT,
                "\"{value}\" should fall back to the default"
            );
        }
    }

    #[test]
    fn reported_units_are_capped() {
        let units: Vec<String> = (0..MAX_REPORTED_UNITS + 3)
            .map(|i| format!("u{i}.service"))
            .collect();
        let reported = report_units(&units);
        assert!(reported.ends_with("(+3 more)"), "{reported}");
        assert_eq!(reported.matches(".service").count(), MAX_REPORTED_UNITS);
    }

    // exact strings are a contract with omnect-os CI
    // (TEST_REBOOT_REASON_CHECK_EXTRA_INFO does an exact match)
    #[test]
    fn extra_info_strings_match_ci_contract() {
        assert_eq!(
            degraded_extra_info(&["dummy-failed.service".to_string()]),
            "system degraded, failed units: dummy-failed.service"
        );
        assert_eq!(
            crash_loop_extra_info(&["dummy-crash-loop.service".to_string()]),
            "crash-looping units: dummy-crash-loop.service"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_is_confirmed_by_consecutive_polls() {
        let script = (0..HEALTH_CONFIRMATION_POLLS)
            .map(|_| healthy_poll())
            .collect();
        let (result, polls) = watch(script, Duration::from_secs(60)).await;
        result.expect("healthy system should validate");
        assert_eq!(polls, HEALTH_CONFIRMATION_POLLS as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fewer_healthy_polls_do_not_confirm() {
        let script = (0..HEALTH_CONFIRMATION_POLLS - 1)
            .map(|_| healthy_poll())
            .collect();
        let (result, _) = watch(script, Duration::from_secs(60)).await;
        assert!(result.is_err(), "health confirmed too early");
    }

    // one poll can land between a unit failing and something restarting it, so a
    // single observation must not roll back the update
    #[tokio::test(flavor = "multi_thread")]
    async fn single_degraded_poll_does_not_roll_back() {
        let mut script = vec![degraded_poll()];
        script.extend((0..HEALTH_CONFIRMATION_POLLS).map(|_| healthy_poll()));
        let (result, _) = watch(script, Duration::from_secs(60)).await;
        result.expect("a single degraded poll should not fail the validation");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn confirmed_degraded_rolls_back_with_the_failed_units() {
        let script = (0..HEALTH_CONFIRMATION_POLLS)
            .map(|_| degraded_poll())
            .collect();
        let (result, _) = watch(script, Duration::from_secs(60)).await;
        assert_eq!(
            result.expect_err("degraded system should fail").to_string(),
            degraded_extra_info(&names(&["a.service"]))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn confirmed_crash_loop_rolls_back_with_the_looping_units() {
        let script = (0..HEALTH_CONFIRMATION_POLLS)
            .map(|_| crash_loop_poll())
            .collect();
        let (result, _) = watch(script, Duration::from_secs(60)).await;
        assert_eq!(
            result.expect_err("crash loop should fail").to_string(),
            crash_loop_extra_info(&names(&["loop.service"]))
        );
    }

    fn tally(observations: &[SystemHealth]) -> HealthTally {
        let mut tally = HealthTally::default();
        for health in observations {
            tally.observe(health);
        }
        tally
    }

    fn degraded() -> SystemHealth {
        SystemHealth::Degraded(names(&["a.service"]))
    }

    #[test]
    fn unconfirmed_unhealthy_polls_do_not_decide_the_deadline() {
        for unhealthy in 1..HEALTH_CONFIRMATION_POLLS {
            let mut observations = vec![SystemHealth::Healthy];
            observations.extend((0..unhealthy).map(|_| degraded()));
            assert_eq!(
                tally(&observations).unhealthy_verdict(),
                None,
                "{unhealthy} degraded polls should not outweigh a healthy one"
            );
        }
    }

    // the deadline verdict must not depend on which poll happened to be last
    #[test]
    fn repeated_unhealthy_polls_decide_the_deadline() {
        let mut observations: Vec<SystemHealth> = (0..HEALTH_CONFIRMATION_POLLS)
            .flat_map(|_| [SystemHealth::Healthy, degraded()])
            .collect();
        for last_health in [SystemHealth::Healthy, degraded()] {
            assert_eq!(
                deadline_verdict(&tally(&observations), Some(&last_health))
                    .expect_err("an alternating system should fail")
                    .to_string(),
                degraded_extra_info(&names(&["a.service"]))
            );
            observations.push(last_health);
        }
    }

    // the count and the reported cause must agree: what was seen most often
    #[test]
    fn the_cause_seen_most_often_is_reported() {
        let observations = [
            SystemHealth::Healthy,
            degraded(),
            SystemHealth::CrashLooping(names(&["loop.service"])),
            degraded(),
        ];
        assert_eq!(
            tally(&observations).unhealthy_verdict(),
            Some(degraded_extra_info(&names(&["a.service"])).as_str())
        );
    }

    // without a healthy observation there is nothing that speaks for the update
    #[test]
    fn a_single_unhealthy_poll_decides_a_deadline_without_healthy_polls() {
        assert_eq!(
            deadline_verdict(&tally(&[degraded()]), Some(&degraded()))
                .expect_err("a system that was never healthy should fail")
                .to_string(),
            degraded_extra_info(&names(&["a.service"]))
        );
    }

    // a system that was up before must not be rolled back because the last poll
    // saw a state the check does not rate, e.g. degraded without a name to report
    #[test]
    fn a_healthy_poll_survives_an_unrated_last_observation() {
        let unrated = SystemHealth::Starting(SYSTEM_STATE_DEGRADED.to_string());
        deadline_verdict(&tally(&[SystemHealth::Healthy]), Some(&unrated))
            .expect("a healthy observation should still count at the deadline");
    }

    #[test]
    fn unconfirmed_degraded_at_the_deadline_does_not_roll_back() {
        let observations = [SystemHealth::Healthy, degraded()];
        deadline_verdict(&tally(&observations), Some(&degraded()))
            .expect("a single degraded poll is no basis for a rollback");
    }

    // alternating failures never confirm one class, so only the deadline ends the
    // wait - it must not poll forever
    #[tokio::test(flavor = "multi_thread")]
    async fn alternating_failures_end_at_the_deadline() {
        let script = vec![degraded_poll(), crash_loop_poll()];
        let (result, polls) = watch(script, Duration::ZERO).await;
        assert!(result.is_err());
        assert_eq!(
            polls, 1,
            "the deadline should end the wait on the first poll"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pending_restart_at_the_deadline_is_accepted() {
        let (result, _) = watch(vec![restarting_poll()], Duration::ZERO).await;
        result.expect("a pending restart is no proof of a crash loop");
    }

    // a bus hiccup while the system is still booting is a single observation
    // like any other and must not roll back the update
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_poll_does_not_end_the_wait() {
        let mut script = vec![failed_poll()];
        script.extend((0..HEALTH_CONFIRMATION_POLLS).map(|_| healthy_poll()));
        let (result, _) = watch(script, Duration::from_secs(60)).await;
        result.expect("a single failed poll should not fail the validation");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeatedly_failed_polls_end_the_wait() {
        let script = (0..HEALTH_CONFIRMATION_POLLS)
            .map(|_| failed_poll())
            .collect();
        let (result, polls) = watch(script, Duration::from_secs(60)).await;
        let error = result.expect_err("a broken bus should fail the validation");
        // the cause has to be in the message the reboot reason takes
        assert_eq!(
            error.to_string(),
            format!("failed to poll system health repeatedly: {POLL_ERROR}")
        );
        assert_eq!(polls, HEALTH_CONFIRMATION_POLLS as usize);
    }

    // a failed poll is no observation, so the confirmation starts over
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_poll_breaks_the_confirmation() {
        let mut script = vec![healthy_poll(), healthy_poll(), failed_poll()];
        script.extend((0..HEALTH_CONFIRMATION_POLLS).map(|_| healthy_poll()));
        let (result, polls) = watch(script, Duration::from_secs(60)).await;
        result.expect("healthy system should validate");
        assert_eq!(polls, 3 + HEALTH_CONFIRMATION_POLLS as usize);
    }

    #[test]
    fn a_deadline_without_any_observation_fails() {
        assert_eq!(
            deadline_verdict(&HealthTally::default(), None)
                .expect_err("a wait without an observation should fail")
                .to_string(),
            "no system health observation within deadline"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn still_starting_at_the_deadline_fails() {
        let script = vec![Ok((SystemState::parse("initializing"), vec![]))];
        let (result, _) = watch(script, Duration::ZERO).await;
        assert_eq!(
            result
                .expect_err("a system that never starts should fail")
                .to_string(),
            "system not healthy within deadline, last state: initializing"
        );
    }
}
