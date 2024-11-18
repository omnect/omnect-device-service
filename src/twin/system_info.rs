use super::web_service;
use super::{feature::*, Feature};
use crate::system;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::{IotHubClient, IotMessage};
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use serde::Serialize;
use serde_json::json;
use std::env;
use std::path::Path;
use sysinfo::Disks;
use sysinfo::{Components, MemoryRefreshKind};
use sysinfo::{CpuRefreshKind, RefreshKind, System};
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tokio::time::Duration;

lazy_static! {
    static ref DEVICE_ID: String =
        std::env::var("IOTEDGE_DEVICEID").unwrap_or("deviceId".to_string());
    static ref IOTHUB: String =
        std::env::var("IOTEDGE_IOTHUBHOSTNAME").unwrap_or("iothubHostName".to_string());
    static ref MODULE_ID: String =
        std::env::var("IOTEDGE_MODULEID").unwrap_or("omnect-device-service".to_string());
}

#[derive(Serialize)]
struct Label {
    edge_device: String,
    iothub: String,
    module_name: String,
}

#[derive(Serialize)]
struct Metric {
    #[serde(rename = "TimeGeneratedUtc")]
    time_generated_utc: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Value")]
    value: f64,
    #[serde(rename = "Labels")]
    labels: Label,
}

impl Metric {
    fn new(time: String, name: &str, value: f64) -> Metric {
        Metric {
            time_generated_utc: time,
            name: name.to_string(),
            value,
            labels: Label {
                edge_device: DEVICE_ID.to_string(),
                iothub: IOTHUB.to_string(),
                module_name: MODULE_ID.to_string(),
            },
        }
    }
}

lazy_static! {
    static ref TIMESYNC_FILE: &'static Path = if cfg!(feature = "mock") {
        Path::new("/tmp/synchronized")
    } else {
        Path::new("/run/systemd/timesync/synchronized")
    };
}

#[derive(Default, Serialize)]
pub struct SystemInfo {
    #[serde(skip_serializing)]
    tx_reported_properties: Option<mpsc::Sender<serde_json::Value>>,
    os: serde_json::Value,
    azure_sdk_version: String,
    omnect_device_service_version: String,
    boot_time: Option<String>,
}

#[async_trait(?Send)]
impl Feature for SystemInfo {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::SYSTEM_INFO_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_SYSTEM_INFO") != Ok("true".to_string())
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: mpsc::Sender<serde_json::Value>,
        tx_outgoing_message: mpsc::Sender<IotMessage>,
    ) -> Result<()> {
        Some(tokio::spawn(SystemInfo::data_collector(
            tx_outgoing_message.clone(),
        )));
        self.tx_reported_properties = Some(tx_reported_properties);
        self.report().await
    }

    async fn connect_web_service(&self) -> Result<()> {
        self.report().await
    }

    fn event_stream(&mut self) -> EventStreamResult {
        if self.boot_time.is_none() {
            Ok(Some(file_created_stream::<SystemInfo>(vec![
                &TIMESYNC_FILE,
            ])))
        } else {
            Ok(None)
        }
    }

    async fn command(&mut self, cmd: Command) -> CommandResult {
        let Command::FileCreated(_) = cmd else {
            bail!("unexpected event: {cmd:?}")
        };

        self.boot_time = Some(system::boot_time()?);
        self.report().await?;

        Ok(None)
    }
}

impl SystemInfo {
    const SYSTEM_INFO_VERSION: u8 = 1;
    const ID: &'static str = "system_info";

    pub fn new() -> Result<Self> {
        let boot_time = if matches!(TIMESYNC_FILE.try_exists(), Ok(true)) {
            Some(system::boot_time()?)
        } else {
            debug!("new: start timesync watcher since not synced yet");
            None
        };

        Ok(SystemInfo {
            tx_reported_properties: None,
            os: system::sw_version()?,
            azure_sdk_version: IotHubClient::sdk_version_string(),
            omnect_device_service_version: env!("CARGO_PKG_VERSION").to_string(),
            boot_time,
        })
    }

    async fn report(&self) -> Result<()> {
        web_service::publish(
            web_service::PublishChannel::SystemInfo,
            serde_json::to_value(self).context("connect_web_service: cannot serialize")?,
        )
        .await;

        let Some(tx) = &self.tx_reported_properties else {
            warn!("report: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({
            "system_info": serde_json::to_value(self).context("report: cannot serialize")?
        }))
        .await
        .context("report: send")
    }

    async fn data_collector(tx_outgoing_message: mpsc::Sender<IotMessage>) {
        // configure interval of wind speed and wind direction samples in seconds
        let mut collect_interval = tokio::time::interval(Duration::from_secs(60));

        let mut components = Components::new_with_refreshed_list();
        let mut s = System::new_with_specifics(
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );
        let mut disks = Disks::new_with_refreshed_list();

        info!("data collector!");

        loop {
            collect_interval.tick().await;

            let time = match time::OffsetDateTime::now_utc().format(&Rfc3339) {
                Ok(time) => time,
                Err(e) => {
                    error!("timestamp could not be generated: {e}");
                    continue;
                }
            };
            components.refresh_list();
            s.refresh_cpu_usage();
            s.refresh_memory();
            disks.refresh();

            // for component in components.iter() {
            //     debug!("{} {}°C", component.label(), component.temperature());
            // }
            let first_component = components.iter().next().unwrap();
            debug!(
                "{} {}°C",
                first_component.label(),
                first_component.temperature()
            );
            for disk in disks.list() {
                debug!("[{:?}] {}B", disk.name(), disk.available_space());
            }

            let metric_list = vec![
                Metric::new(
                    time.clone(),
                    "temperature",
                    first_component.temperature() as f64,
                ),
                Metric::new(time.clone(), "cpu_usage", s.global_cpu_usage() as f64),
                Metric::new(time.clone(), "ram_used", s.used_memory() as f64),
                Metric::new(time.clone(), "ram_total", s.total_memory() as f64),
            ];

            match serde_json::to_vec(&metric_list) {
                Ok(json) => {
                    match IotMessage::builder()
                        .set_body(json)
                        .set_content_type("application/json")
                        .set_content_encoding("utf-8")
                        .set_output_queue("metrics")
                        .build()
                    {
                        Ok(msg) => {
                            let _ = tx_outgoing_message.send(msg).await;
                        }
                        Err(e) => error!("telemetry message could not be transmitted: {e}"),
                    }
                }
                Err(e) => error!("metrics list could not be converted to vector: {e}"),
            }
        }
    }
}
