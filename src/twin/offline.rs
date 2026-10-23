use crate::systemd::networkd::DEFAULT_WAIT_ONLINE_TIMEOUT;
use azure_iot_sdk::client::UnauthenticatedReason;
use std::time::{Duration, Instant};

// systemd-networkd-wait-online reboots a device that is not online when its
// timeout ends at boot, so the boot warning must come before that.
const BOOT_WARNING_DIVISOR: u32 = 2;

pub fn offline_limit(wait_online_timeout: anyhow::Result<Option<Duration>>) -> Option<Duration> {
    wait_online_timeout.unwrap_or(Some(DEFAULT_WAIT_ONLINE_TIMEOUT))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Since {
    Boot,
    ProcessStart,
    ConnectionLost,
}

pub struct OfflineTracker {
    limit: Option<Duration>,
    online: bool,
    deadline: Option<(Instant, Since)>,
    reason: Option<UnauthenticatedReason>,
}

impl OfflineTracker {
    /// A process started before the boot warning waits for it; a later one
    /// waits `limit` from its start.
    pub fn new(now: Instant, limit: Option<Duration>, uptime: Duration) -> Self {
        let deadline = limit.and_then(|limit| {
            let (delay, since) = match (limit / BOOT_WARNING_DIVISOR).checked_sub(uptime) {
                Some(delay) if !delay.is_zero() => (delay, Since::Boot),
                _ => (limit, Since::ProcessStart),
            };
            Some((now.checked_add(delay)?, since))
        });
        Self {
            limit,
            online: false,
            deadline,
            reason: None,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline.map(|(at, _)| at)
    }

    pub fn connected(&mut self) {
        self.online = true;
        self.deadline = None;
        self.reason = None;
    }

    pub fn disconnected(&mut self, now: Instant, reason: UnauthenticatedReason) {
        self.reason = Some(reason);
        if self.online {
            self.online = false;
            self.deadline = self
                .limit
                .and_then(|limit| Some((now.checked_add(limit)?, Since::ConnectionLost)));
        }
    }

    /// The warning of a passed deadline. There is one deadline per outage.
    pub fn expired(&mut self, now: Instant) -> Option<String> {
        let (_, since) = self.deadline.filter(|(at, _)| *at <= now)?;
        self.deadline = None;
        let limit = self.limit?;
        let reason = self
            .reason
            .map(|r| format!(" (last reason: {r:?})"))
            .unwrap_or_default();
        Some(match since {
            Since::Boot => format!(
                "boot: failed to connect to iothub within {} s after boot{reason}; systemd-networkd-wait-online reboots the device if the network is not online within its timeout ({} s, OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS)",
                (limit / BOOT_WARNING_DIVISOR).as_secs(),
                limit.as_secs()
            ),
            Since::ProcessStart => format!(
                "runtime: failed to connect to iothub for {} s since this process started{reason}",
                limit.as_secs()
            ),
            Since::ConnectionLost => format!(
                "runtime: failed to connect to iothub for {} s since the connection was lost{reason}",
                limit.as_secs()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use UnauthenticatedReason::NoNetwork;

    const T: Duration = Duration::from_secs(300);
    const EARLY: Duration = Duration::from_secs(10);
    const LATE: Duration = Duration::from_secs(600);

    #[test]
    fn limit_is_the_wait_online_timeout() {
        assert_eq!(offline_limit(Ok(Some(T))), Some(T));
        assert_eq!(offline_limit(Ok(None)), None);
        assert_eq!(
            offline_limit(Err(anyhow::anyhow!("service file missing"))),
            Some(DEFAULT_WAIT_ONLINE_TIMEOUT)
        );
    }

    #[test]
    fn early_process_waits_for_half_the_limit_after_boot() {
        let start = Instant::now();
        let t = OfflineTracker::new(start, Some(T), EARLY);
        assert_eq!(t.deadline(), Some(start + Duration::from_secs(140)));
    }

    #[test]
    fn late_process_waits_the_limit_from_its_start() {
        let start = Instant::now();
        let at_half = OfflineTracker::new(start, Some(T), T / BOOT_WARNING_DIVISOR);
        assert_eq!(at_half.deadline(), Some(start + T));
        let late = OfflineTracker::new(start, Some(T), LATE);
        assert_eq!(late.deadline(), Some(start + T));
    }

    #[test]
    fn odd_limit_keeps_the_fraction() {
        let start = Instant::now();
        let odd = Duration::from_secs(301);
        let t = OfflineTracker::new(start, Some(odd), Duration::ZERO);
        assert_eq!(t.deadline(), Some(start + Duration::from_millis(150_500)));
    }

    #[test]
    fn no_limit_never_warns() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, None, EARLY);
        assert_eq!(t.deadline(), None);
        t.connected();
        t.disconnected(start, NoNetwork);
        assert_eq!(t.deadline(), None);
        assert_eq!(t.expired(start + LATE), None);
    }

    #[test]
    fn huge_limit_has_no_deadline() {
        let start = Instant::now();
        let huge = Some(Duration::from_secs(u64::MAX));
        let mut t = OfflineTracker::new(start, huge, EARLY);
        assert_eq!(t.deadline(), None);
        t.connected();
        t.disconnected(start, NoNetwork);
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn boot_warning_once_without_a_connect() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, Some(T), EARLY);
        t.disconnected(start, NoNetwork);
        let at = t.deadline().expect("boot deadline");
        assert_eq!(t.expired(at - EARLY), None);
        assert_eq!(
            t.expired(at).as_deref(),
            Some(
                "boot: failed to connect to iothub within 150 s after boot (last reason: NoNetwork); systemd-networkd-wait-online reboots the device if the network is not online within its timeout (300 s, OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS)"
            )
        );
        t.disconnected(at, NoNetwork);
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn connect_before_the_boot_warning_cancels_it() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, Some(T), EARLY);
        t.connected();
        assert_eq!(t.deadline(), None);
        assert_eq!(t.expired(start + LATE), None);
    }

    #[test]
    fn late_process_warns_without_a_callback() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, Some(T), LATE);
        assert_eq!(
            t.expired(start + T).as_deref(),
            Some("runtime: failed to connect to iothub for 300 s since this process started")
        );
    }

    #[test]
    fn lost_connection_warns_once_per_outage() {
        let start = Instant::now();
        let mut t = OfflineTracker::new(start, Some(T), EARLY);
        t.connected();
        let lost = start + T;
        t.disconnected(lost, NoNetwork);
        t.disconnected(lost + EARLY, NoNetwork);
        assert_eq!(t.deadline(), Some(lost + T));
        assert_eq!(
            t.expired(lost + T).as_deref(),
            Some(
                "runtime: failed to connect to iothub for 300 s since the connection was lost (last reason: NoNetwork)"
            )
        );
        t.disconnected(lost + T * 2, NoNetwork);
        assert_eq!(t.deadline(), None);

        t.connected();
        let again = lost + T * 4;
        t.disconnected(again, NoNetwork);
        assert_eq!(t.deadline(), Some(again + T));
    }
}
