// How long the device may stay without an iothub connection before a warning
// says so. The limit is the wait-online timeout (OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS):
// at boot, a device that is not online by then is rebooted, so the boot warning
// comes at half of it; after boot nothing reboots the device, so a lost
// connection warns once it has lasted the whole limit. A timeout of 0 means the
// device may stay offline, so it never warns.
use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

const DEFAULT_LIMIT: Duration = Duration::from_secs(300);

pub fn offline_limit(wait_online_timeout: anyhow::Result<Option<Duration>>) -> Option<Duration> {
    wait_online_timeout.unwrap_or(Some(DEFAULT_LIMIT))
}

/// How long from now until the boot warning, if this process started before it.
pub fn boot_warning_delay(limit: Option<Duration>, uptime: Duration) -> Option<Duration> {
    let at = limit? / 2;
    (uptime < at).then(|| at - uptime)
}

pub fn boot_message(limit: Duration) -> String {
    let limit = limit.as_secs();
    format!(
        "boot: failed to connect to iothub within {} s after boot; if the network is not online at {limit} s after boot, systemd-networkd-wait-online reboots the device (OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS={limit})",
        limit / 2
    )
}

pub struct OfflineTracker {
    started: Instant,
    has_boot_warning: bool,
    connected_once: bool,
    since: Option<Instant>,
    warned: bool,
}

impl OfflineTracker {
    pub fn new(started: Instant, has_boot_warning: bool) -> Self {
        Self {
            started,
            has_boot_warning,
            connected_once: false,
            since: None,
            warned: false,
        }
    }

    pub fn connected_once(&self) -> bool {
        self.connected_once
    }

    pub fn connected(&mut self) {
        self.connected_once = true;
        self.since = None;
        self.warned = false;
    }

    /// The runtime warning, once per outage, when the outage has lasted `limit`.
    pub fn disconnected(
        &mut self,
        now: Instant,
        limit: Option<Duration>,
        reason: impl Debug,
    ) -> Option<String> {
        // until the first connect, the boot warning speaks for this process
        if !self.connected_once && self.has_boot_warning {
            return None;
        }
        let limit = limit?;
        let (since, context) = if self.connected_once {
            (*self.since.get_or_insert(now), "the connection was lost")
        } else {
            (self.started, "this process started")
        };
        let down = now.saturating_duration_since(since);
        if self.warned || down < limit {
            return None;
        }
        self.warned = true;
        Some(format!(
            "runtime: failed to connect to iothub for {} s since {context} ({reason:?}); the wait-online timeout ({} s) applies only at boot, so no reboot follows",
            down.as_secs(),
            limit.as_secs()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    enum Reason {
        NoNetwork,
    }
    use Reason::NoNetwork;

    const T: Duration = Duration::from_secs(300);

    #[test]
    fn limit_is_the_wait_online_timeout() {
        assert_eq!(offline_limit(Ok(Some(T))), Some(T));
        assert_eq!(offline_limit(Ok(None)), None);
        assert_eq!(
            offline_limit(Err(anyhow::anyhow!("service file missing"))),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn boot_warning_at_half_the_limit_after_boot() {
        assert_eq!(
            boot_warning_delay(Some(T), Duration::from_secs(10)),
            Some(Duration::from_secs(140))
        );
        assert_eq!(boot_warning_delay(Some(T), Duration::from_secs(150)), None);
        assert_eq!(boot_warning_delay(Some(T), Duration::from_secs(600)), None);
        assert_eq!(boot_warning_delay(None, Duration::from_secs(10)), None);
    }

    #[test]
    fn boot_message_names_its_context() {
        assert_eq!(
            boot_message(T),
            "boot: failed to connect to iothub within 150 s after boot; if the network is not online at 300 s after boot, systemd-networkd-wait-online reboots the device (OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS=300)"
        );
    }

    #[test]
    fn no_runtime_warning_while_the_boot_warning_covers_the_process() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, true);
        assert_eq!(t.disconnected(start + T * 2, Some(T), NoNetwork), None);
    }

    #[test]
    fn lost_connection_warns_once_per_outage() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, true);
        t.connected();
        let lost = start + T;
        assert_eq!(t.disconnected(lost, Some(T), NoNetwork), None);
        assert_eq!(
            t.disconnected(lost + T, Some(T), NoNetwork).as_deref(),
            Some(
                "runtime: failed to connect to iothub for 300 s since the connection was lost (NoNetwork); the wait-online timeout (300 s) applies only at boot, so no reboot follows"
            )
        );
        assert_eq!(t.disconnected(lost + T * 3, Some(T), NoNetwork), None);
        t.connected();
        let again = lost + T * 4;
        assert_eq!(t.disconnected(again, Some(T), NoNetwork), None);
        assert!(t.disconnected(again + T, Some(T), NoNetwork).is_some());
    }

    #[test]
    fn restarted_process_counts_from_its_start() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, false);
        assert_eq!(
            t.disconnected(start + Duration::from_secs(5), Some(T), NoNetwork),
            None
        );
        assert_eq!(
            t.disconnected(start + T, Some(T), NoNetwork).as_deref(),
            Some(
                "runtime: failed to connect to iothub for 300 s since this process started (NoNetwork); the wait-online timeout (300 s) applies only at boot, so no reboot follows"
            )
        );
    }

    #[test]
    fn offline_scenario_never_warns() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, false);
        assert_eq!(t.disconnected(start + T * 10, None, NoNetwork), None);
    }
}
