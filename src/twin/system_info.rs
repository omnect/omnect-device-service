use super::web_service;
use super::{feature::*, Feature};
use crate::system;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::{IotHubClient, IotMessage};
use futures::StreamExt;
use lazy_static::lazy_static;
use log::{debug, info, warn};
use serde::Serialize;
use serde_json::json;
use std::env;
use std::path::Path;
use sysinfo::{Components, CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

lazy_static! {
    static ref REFRESH_SYSTEM_INFO_INTERVAL_SECS: u64 = {
        const REFRESH_SYSTEM_INFO_INTERVAL_SECS_DEFAULT: &str = "60";
        std::env::var("REFRESH_SYSTEM_INFO_INTERVAL_SECS")
            .unwrap_or(REFRESH_SYSTEM_INFO_INTERVAL_SECS_DEFAULT.to_string())
            .parse::<u64>()
            .expect("cannot parse REFRESH_SYSTEM_INFO_INTERVAL_SECS env var")
    };
}

#[derive(Serialize)]
struct Label {
    device_id: String,
    module_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sensor: Option<String>,
}

#[derive(Serialize)]
struct Metric {
    time_generated_utc: String,
    name: String,
    value: f64,
    labels: Label,
}

impl Metric {
    fn new(
        time: String,
        name: String,
        value: f64,
        device: String,
        sensor: Option<String>,
    ) -> Metric {
        Metric {
            time_generated_utc: time,
            name,
            value,
            labels: Label {
                device_id: device,
                module_name: "omnect-device-service".to_string(),
                sensor,
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
pub struct Info {
    os: serde_json::Value,
    azure_sdk_version: String,
    omnect_device_service_version: String,
    boot_time: Option<String>,
}

#[derive(Default)]
pub struct SystemInfo {
    tx_reported_properties: Option<mpsc::Sender<serde_json::Value>>,
    tx_outgoing_message: Option<mpsc::Sender<IotMessage>>,
    info: Info,
    sysinfo_components: Components,
    sysinfo_disk: Disks,
    sysinfo_memory: System,
    hostname: String,
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
        self.tx_outgoing_message = Some(tx_outgoing_message);
        self.tx_reported_properties = Some(tx_reported_properties);
        self.report().await
    }

    async fn connect_web_service(&self) -> Result<()> {
        self.report().await
    }

    fn event_stream(&mut self) -> EventStreamResult {
        Ok(match *REFRESH_SYSTEM_INFO_INTERVAL_SECS {
            0 if self.info.boot_time.is_none() => {
                Some(file_created_stream::<SystemInfo>(vec![&TIMESYNC_FILE]))
            }
            0 if self.info.boot_time.is_some() => None,
            _ => {
                let interval_stream = interval_stream::<SystemInfo>(interval(Duration::from_secs(
                    *REFRESH_SYSTEM_INFO_INTERVAL_SECS,
                )));
                Some(if self.info.boot_time.is_none() {
                    futures::stream::select(
                        interval_stream,
                        file_created_stream::<SystemInfo>(vec![&TIMESYNC_FILE]),
                    )
                    .boxed()
                } else {
                    interval_stream
                })
            }
        })
    }

    async fn command(&mut self, cmd: Command) -> CommandResult {
        match cmd {
            Command::Interval(_) => {
                self.metrics().await?;
            }
            Command::FileCreated(_) => {
                self.info.boot_time = Some(system::boot_time()?);
            }
            _ => bail!("unexpected command"),
        }
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

        let Some(hostname) = System::host_name() else {
            bail!("metrics: hostname could not be read")
        };

        Ok(SystemInfo {
            tx_reported_properties: None,
            tx_outgoing_message: None,
            info: Info {
                os: system::sw_version()?,
                azure_sdk_version: IotHubClient::sdk_version_string(),
                omnect_device_service_version: env!("CARGO_PKG_VERSION").to_string(),
                boot_time,
            },
            hostname,
            sysinfo_components: Components::new_with_refreshed_list(),
            sysinfo_disk: Disks::new_with_refreshed_list(),
            sysinfo_memory: System::new_with_specifics(
                RefreshKind::new()
                    .with_cpu(CpuRefreshKind::everything())
                    .with_memory(MemoryRefreshKind::everything()),
            ),
        })
    }

    async fn report(&self) -> Result<()> {
        web_service::publish(
            web_service::PublishChannel::SystemInfo,
            serde_json::to_value(&self.info).context("connect_web_service: cannot serialize")?,
        )
        .await;

        let Some(tx) = &self.tx_reported_properties else {
            warn!("report: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({
            "system_info": serde_json::to_value(&self.info).context("report: cannot serialize")?
        }))
        .await
        .context("report: send")
    }

    fn cpu_usage(&self, time: String) -> Metric {
        Metric::new(
            time,
            "cpu_usage".to_string(),
            self.sysinfo_memory.global_cpu_usage() as f64,
            self.hostname.clone(),
            None,
        )
    }

    fn memory_used(&self, time: String) -> Metric {
        Metric::new(
            time,
            "memory_used".to_string(),
            self.sysinfo_memory.used_memory() as f64,
            self.hostname.clone(),
            None,
        )
    }

    fn memory_total(&self, time: String) -> Metric {
        Metric::new(
            time,
            "memory_total".to_string(),
            self.sysinfo_memory.total_memory() as f64,
            self.hostname.clone(),
            None,
        )
    }

    fn disk_used(&self, time: String, value: f64) -> Metric {
        Metric::new(
            time,
            "disk_used".to_string(),
            value,
            self.hostname.clone(),
            None,
        )
    }

    fn disk_total(&self, time: String, value: f64) -> Metric {
        Metric::new(
            time,
            "disk_total".to_string(),
            value,
            self.hostname.clone(),
            None,
        )
    }

    fn temp(&self, time: String, value: f64, sensor: String) -> Metric {
        Metric::new(
            time,
            "temp".to_string(),
            value,
            self.hostname.clone(),
            Some(sensor),
        )
    }

    async fn metrics(&mut self) -> Result<()> {
        let Some(tx) = &self.tx_outgoing_message else {
            warn!("metrics: skip since tx_outgoing_message is None");
            return Ok(());
        };

        let Ok(time) = time::OffsetDateTime::now_utc().format(&Rfc3339) else {
            bail!("metrics: timestamp could not be generated")
        };

        self.sysinfo_components.refresh_list();
        self.sysinfo_memory.refresh_cpu_usage();
        self.sysinfo_memory.refresh_memory();
        self.sysinfo_disk.refresh();

        let mut disk_total = 0;
        let mut disk_used = 0;
        for disk in self.sysinfo_disk.list() {
            if disk.name().to_str() == Some("/dev/omnect/data") {
                disk_total = disk.total_space();
                disk_used = disk.total_space() - disk.available_space();
                break;
            }
        }

        let mut metric_list = vec![
            Self::cpu_usage(self, time.clone()),
            Self::memory_used(self, time.clone()),
            Self::memory_total(self, time.clone()),
            Self::disk_used(self, time.clone(), disk_used as f64),
            Self::disk_total(self, time.clone(), disk_total as f64),
        ];

        for component in self.sysinfo_components.iter() {
            metric_list.push(Self::temp(
                self,
                time.clone(),
                component.temperature() as f64,
                format!("{}", component.label()),
            ));
        }

        let json = serde_json::to_vec(&metric_list)
            .context("metrics list could not be converted to vector:")?;

        let msg = IotMessage::builder()
            .set_body(json)
            .set_content_type("application/json")
            .set_content_encoding("utf-8")
            .set_output_queue("metrics")
            .build()
            .context("telemetry message could not be transmitted")?;

        tx.send(msg).await?;

        info!("metrics: telemetry message transmitted");

        Ok(())
    }
}
