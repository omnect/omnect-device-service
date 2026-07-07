use crate::twin::{Feature, feature::*};
use anyhow::{Context, Result, bail};
use azure_iot_sdk::client::IotMessage;
use log::{debug, warn};
use serde::Serialize;
use serde_json::json;
use std::env;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

const ID: &str = "wifi_commissioning";
const WIFI_COMMISSIONING_VERSION: u8 = 1;

#[derive(Debug, PartialEq, Serialize)]
struct WifiCommissioningReport {
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    ble_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interface: Option<String>,
}

#[cfg(any(all(feature = "wifi_commissioning", not(feature = "mock")), test))]
const UNIT_NAME_PREFIX: &str = "wifi-commissioning-service@";

#[cfg(any(all(feature = "wifi_commissioning", not(feature = "mock")), test))]
fn iface_from_unit_name(unit: &str) -> Option<String> {
    unit.strip_prefix(UNIT_NAME_PREFIX)?
        .strip_suffix(".service")
        .map(str::to_string)
}

#[cfg(any(all(feature = "wifi_commissioning", not(feature = "mock")), test))]
fn parse_ble_enabled(response: &str) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct ServiceInfo {
        ble_enabled: bool,
    }
    let (_headers, body) = response
        .split_once("\r\n\r\n")
        .context("service-info response missing body")?;
    Ok(serde_json::from_str::<ServiceInfo>(body)
        .context("parse service-info body")?
        .ble_enabled)
}

// `None` means the service is not available: either no active instance, or an
// active instance whose service-info API could not be reached.
fn build_report(state: Option<(String, bool)>) -> WifiCommissioningReport {
    match state {
        Some((interface, ble_enabled)) => WifiCommissioningReport {
            running: true,
            ble_enabled: Some(ble_enabled),
            interface: Some(interface),
        },
        None => WifiCommissioningReport {
            running: false,
            ble_enabled: None,
            interface: None,
        },
    }
}

// only report on change
async fn send_report(
    tx: &Option<Sender<serde_json::Value>>,
    last: &mut Option<WifiCommissioningReport>,
    force: bool,
    report: WifiCommissioningReport,
) -> Result<()> {
    if !force && last.as_ref() == Some(&report) {
        debug!("wifi_commissioning: state unchanged");
        return Ok(());
    }

    let value = json!({
        "wifi_commissioning": serde_json::to_value(&report).context("serialize report")?
    });
    *last = Some(report);

    let Some(tx) = tx else {
        warn!("wifi_commissioning: skip report, tx_reported_properties is None");
        return Ok(());
    };

    tx.send(value)
        .await
        .context("send wifi_commissioning report")
}

#[derive(Default)]
pub struct WifiCommissioning {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    last: Option<WifiCommissioningReport>,
}

impl WifiCommissioning {
    async fn report(&mut self, force: bool) -> Result<()> {
        #[cfg(all(feature = "wifi_commissioning", not(feature = "mock")))]
        let state = live::observe().await?;
        #[cfg(not(all(feature = "wifi_commissioning", not(feature = "mock"))))]
        let state: Option<(String, bool)> = None;

        send_report(
            &self.tx_reported_properties,
            &mut self.last,
            force,
            build_report(state),
        )
        .await
    }
}

impl Feature for WifiCommissioning {
    fn name(&self) -> String {
        ID.to_string()
    }

    fn version(&self) -> u8 {
        WIFI_COMMISSIONING_VERSION
    }

    fn is_enabled(&self) -> bool {
        match env::var("DISTRO_FEATURES") {
            Ok(features) => features.split_whitespace().any(|feature| feature == "wifi"),
            _ => false,
        }
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.tx_reported_properties = Some(tx_reported_properties);
        self.report(true).await
    }

    #[cfg(all(feature = "wifi_commissioning", not(feature = "mock")))]
    fn command_request_stream(&mut self, cancel: CancellationToken) -> CommandRequestStreamResult {
        Ok(Some(
            debounced_command_stream::<_, _, _, WifiCommissioning>(
                live::signal_stream(),
                COMMAND_EVENT_DEBOUNCE,
                cancel,
            ),
        ))
    }

    #[cfg(not(all(feature = "wifi_commissioning", not(feature = "mock"))))]
    fn command_request_stream(&mut self, _cancel: CancellationToken) -> CommandRequestStreamResult {
        Ok(None)
    }

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        let Command::Tick(_) = cmd else {
            bail!("unexpected command: {cmd:?}")
        };
        self.report(false).await?;
        Ok(None)
    }
}

#[cfg(all(feature = "wifi_commissioning", not(feature = "mock")))]
mod live {
    use super::*;
    use crate::systemd::unit;
    use futures::StreamExt;
    use log::error;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    const UNIT_PATTERN: &str = "wifi-commissioning-service@*.service";
    const SERVICE_INFO_REQUEST: &str =
        "GET /api/v1/service-info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

    // `None` → not available. An active unit whose service-info API cannot be
    // reached is treated as not available and logged as an error, since its BLE
    // state is unknown.
    pub(super) async fn observe() -> Result<Option<(String, bool)>> {
        let Some(iface) = active_iface().await? else {
            return Ok(None);
        };

        match query_ble_enabled(&iface).await {
            Ok(ble_enabled) => Ok(Some((iface, ble_enabled))),
            Err(e) => {
                error!("wifi_commissioning: {iface} active but service-info query failed: {e:#}");
                Ok(None)
            }
        }
    }

    async fn active_iface() -> Result<Option<String>> {
        let names = unit::active_units_by_patterns(&[UNIT_PATTERN]).await?;
        if names.len() > 1 {
            warn!("wifi_commissioning: multiple active instances {names:?}, using first");
        }
        Ok(names
            .into_iter()
            .next()
            .and_then(|name| iface_from_unit_name(&name)))
    }

    async fn query_ble_enabled(iface: &str) -> Result<bool> {
        let path = format!("/run/wifi-commissioning-service/{iface}/api.sock");

        let raw = tokio::time::timeout(QUERY_TIMEOUT, async {
            let mut stream = UnixStream::connect(&path)
                .await
                .with_context(|| format!("connect {path}"))?;
            stream
                .write_all(SERVICE_INFO_REQUEST.as_bytes())
                .await
                .context("write service-info request")?;
            let mut buf = Vec::new();
            stream
                .read_to_end(&mut buf)
                .await
                .context("read service-info response")?;
            anyhow::Ok(buf)
        })
        .await
        .context("service-info query timed out")??;

        let text = String::from_utf8(raw).context("service-info response not utf-8")?;
        parse_ble_enabled(&text)
    }

    // Re-evaluate only when a wifi-commissioning-service job completes.
    pub(super) async fn signal_stream() -> Result<impl futures::Stream<Item = ()> + Send> {
        Ok(unit::job_removed_units()
            .await?
            .filter_map(|name| async move { name.starts_with(UNIT_NAME_PREFIX).then_some(()) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

    #[test]
    fn iface_from_unit_name_matches() {
        assert_eq!(
            iface_from_unit_name("wifi-commissioning-service@wlan0.service"),
            Some("wlan0".to_string())
        );
    }

    #[test]
    fn iface_from_unit_name_rejects_non_matching() {
        assert_eq!(iface_from_unit_name("other.service"), None);
    }

    #[test]
    fn parse_ble_enabled_reads_field() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                  {\"status\":\"ok\",\"ble_enabled\":true,\"interface_name\":\"wlan0\",\"version\":\"0.2.0\"}";
        assert!(parse_ble_enabled(ok).expect("parse"));

        let off = "HTTP/1.1 200 OK\r\n\r\n{\"ble_enabled\":false}";
        assert!(!parse_ble_enabled(off).expect("parse"));
    }

    #[test]
    fn parse_ble_enabled_errors_without_body() {
        assert!(parse_ble_enabled("HTTP/1.1 200 OK\r\nContent-Type: text/plain").is_err());
    }

    #[test]
    fn build_report_not_available() {
        assert_eq!(
            serde_json::to_value(build_report(None)).expect("serialize"),
            json!({ "running": false })
        );
    }

    #[test]
    fn build_report_available_reports_ble_and_interface() {
        assert_eq!(
            serde_json::to_value(build_report(Some(("wlan0".to_string(), true))))
                .expect("serialize"),
            json!({ "running": true, "ble_enabled": true, "interface": "wlan0" })
        );
        assert_eq!(
            serde_json::to_value(build_report(Some(("wlan0".to_string(), false))))
                .expect("serialize"),
            json!({ "running": true, "ble_enabled": false, "interface": "wlan0" })
        );
    }

    #[tokio::test]
    async fn send_report_emits_and_records() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut last = None;
        send_report(
            &Some(tx),
            &mut last,
            true,
            build_report(Some(("wlan0".to_string(), true))),
        )
        .await
        .expect("send");
        assert_eq!(
            rx.recv().await.expect("recv"),
            json!({ "wifi_commissioning": { "running": true, "ble_enabled": true, "interface": "wlan0" } })
        );
        assert!(last.is_some());
    }

    #[tokio::test]
    async fn send_report_skips_unchanged() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut last = None;
        let r = || build_report(Some(("wlan0".to_string(), true)));
        send_report(&Some(tx.clone()), &mut last, true, r())
            .await
            .expect("send1");
        rx.recv().await.expect("recv1");
        send_report(&Some(tx), &mut last, false, r())
            .await
            .expect("send2");
        assert!(rx.try_recv().is_err(), "unchanged state must not re-report");
    }

    #[tokio::test]
    async fn send_report_reports_ble_transition() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut last = None;
        send_report(
            &Some(tx.clone()),
            &mut last,
            true,
            build_report(Some(("wlan0".to_string(), false))),
        )
        .await
        .expect("s1");
        rx.recv().await.expect("r1");
        // BLE flips on while the unit keeps running → re-report even with force=false
        send_report(
            &Some(tx),
            &mut last,
            false,
            build_report(Some(("wlan0".to_string(), true))),
        )
        .await
        .expect("s2");
        assert_eq!(
            rx.recv().await.expect("r2"),
            json!({ "wifi_commissioning": { "running": true, "ble_enabled": true, "interface": "wlan0" } })
        );
    }
}
