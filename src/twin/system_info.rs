use crate::{common::RootPartition, reboot_reason, twin::feature::*, web_service};
use anyhow::{Context, Result, bail};
use azure_iot_sdk::client::{IotHubClient, IotMessage};
use inotify::WatchMask;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{env, path::Path, time::Duration};
use sysinfo;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
#[cfg(not(feature = "mock"))]
static TIMESYNC_FILE: &str = "/run/systemd/timesync/synchronized";
#[cfg(feature = "mock")]
static TIMESYNC_FILE: &str = "/tmp/synchronized";

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

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FleetIdCommand {
    pub fleet_id: String,
}

#[derive(Default, Serialize)]
struct OsInfo {
    name: String,
    version: String,
}

#[derive(Default, Serialize)]
struct SoftwareInfo {
    os: OsInfo,
    azure_sdk_version: String,
    omnect_device_service_version: String,
    boot_time: Option<String>,
}

#[derive(Default)]
struct HardwareInfo {
    components: sysinfo::Components,
    disk: sysinfo::Disks,
    system: sysinfo::System,
    hostname: String,
}

#[derive(Default, Serialize)]
pub struct SystemInfo {
    #[serde(skip_serializing)]
    tx_reported_properties: Option<mpsc::Sender<serde_json::Value>>,
    #[serde(skip_serializing)]
    tx_outgoing_message: Option<mpsc::Sender<IotMessage>>,
    #[serde(flatten)]
    software_info: SoftwareInfo,
    #[serde(skip_serializing)]
    hardware_info: HardwareInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    reboot_reason: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fleet_id: Option<String>,
}
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

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        match cmd {
            Command::Interval(_) => {
                self.metrics().await?;
            }
            Command::WatchPath(_) => {
                self.software_info.boot_time = Some(Self::boot_time()?);
                self.report().await?;
            }
            Command::FleetId(id) => {
                self.fleet_id = Some(id.fleet_id.clone());
                web_service::publish(
                    web_service::PublishChannel::SystemInfoV1,
                    serde_json::to_value(self).context("report: cannot serialize")?,
                )
                .await;
            }
            _ => bail!("unexpected command: {cmd:?}"),
        }

        Ok(None)
    }
}

impl SystemInfo {
    const SYSTEM_INFO_VERSION: u8 = 1;
    const ID: &'static str = "system_info";

    pub async fn new() -> Result<Self> {
        let azure_sdk_version = IotHubClient::sdk_version_string();
        let omnect_device_service_version = env!("CARGO_PKG_VERSION").to_string();

        info!(
            "module version: {omnect_device_service_version} ({})",
            env!("GIT_SHORT_REV")
        );
        info!("azure sdk version: {azure_sdk_version}");
        info!(
            "bootloader was updated: {}",
            SystemInfo::bootloader_updated()
        );
        info!(
            "device booted from root {}.",
            RootPartition::current()?.as_str()
        );

        let time_synced_file = Path::new(TIMESYNC_FILE);
        let wd = add_fs_watch::<Self>(
            time_synced_file
                .parent()
                .context("failed to get parent of TIMESYNC_FILE")?,
            WatchMask::CREATE,
        )
        .await?;

        let boot_time = if matches!(time_synced_file.try_exists(), Ok(true)) {
            remove_fs_watch(wd).await?;
            Some(Self::boot_time()?)
        } else {
            None
        };

        let refresh_interval = env::var("REFRESH_SYSTEM_INFO_INTERVAL_SECS")
            .unwrap_or("60".to_string())
            .parse::<u64>()
            .context("cannot parse REFRESH_SYSTEM_INFO_INTERVAL_SECS env var")?;

        if 0 < refresh_interval {
            add_notify_interval::<Self>(Duration::from_secs(refresh_interval)).await?;
        }

        let Some(hostname) = sysinfo::System::host_name() else {
            bail!("metrics: hostname could not be read")
        };

        Ok(SystemInfo {
            tx_reported_properties: None,
            tx_outgoing_message: None,
            software_info: SoftwareInfo {
                os: Self::os_info()?,
                azure_sdk_version,
                omnect_device_service_version,
                boot_time,
            },
            hardware_info: HardwareInfo {
                components: sysinfo::Components::new_with_refreshed_list(),
                disk: sysinfo::Disks::new_with_refreshed_list(),
                system: sysinfo::System::new_with_specifics(
                    sysinfo::RefreshKind::nothing()
                        .with_cpu(sysinfo::CpuRefreshKind::everything())
                        .with_memory(sysinfo::MemoryRefreshKind::everything()),
                ),
                hostname,
            },
            reboot_reason: reboot_reason::current_reboot_reason(),
            fleet_id: None,
        })
    }

    async fn report(&self) -> Result<()> {
        let value = serde_json::to_value(self).context("report: cannot serialize")?;

        web_service::publish(web_service::PublishChannel::SystemInfoV1, value.clone()).await;

        let Some(tx) = &self.tx_reported_properties else {
            warn!("report: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({ "system_info": value }))
            .await
            .context("report: send")
    }

    #[cfg(not(feature = "mock"))]
    fn os_info() -> Result<OsInfo> {
        let os_info = OsInfo {
            name: sysinfo::System::name().context("metrics: os_name could not be read")?,
            version: sysinfo::System::os_version()
                .context("metrics: os_version could not be read")?,
        };
        Ok(os_info)
    }

    #[cfg(feature = "mock")]
    fn os_info() -> Result<OsInfo> {
        let os_info = OsInfo {
            name: "OMNECT-gateway-devel".to_string(),
            version: "4.0.17.123456".to_string(),
        };
        Ok(os_info)
    }

    fn boot_time() -> Result<String> {
        let boot_time = time::OffsetDateTime::UNIX_EPOCH
            + std::time::Duration::from_secs(sysinfo::System::boot_time());
        boot_time
            .format(&Rfc3339)
            .context("boot_time: format uptime")
    }

    fn cpu_usage(&self, time: String) -> Metric {
        Metric::new(
            time,
            "cpu_usage".to_string(),
            self.hardware_info.system.global_cpu_usage() as f64,
            self.hardware_info.hostname.clone(),
            None,
        )
    }

    fn memory_used(&self, time: String) -> Metric {
        Metric::new(
            time,
            "memory_used".to_string(),
            self.hardware_info.system.used_memory() as f64,
            self.hardware_info.hostname.clone(),
            None,
        )
    }

    fn memory_total(&self, time: String) -> Metric {
        Metric::new(
            time,
            "memory_total".to_string(),
            self.hardware_info.system.total_memory() as f64,
            self.hardware_info.hostname.clone(),
            None,
        )
    }

    fn disk_used(&self, time: String, value: f64) -> Metric {
        Metric::new(
            time,
            "disk_used".to_string(),
            value,
            self.hardware_info.hostname.clone(),
            None,
        )
    }

    fn disk_total(&self, time: String, value: f64) -> Metric {
        Metric::new(
            time,
            "disk_total".to_string(),
            value,
            self.hardware_info.hostname.clone(),
            None,
        )
    }

    fn temp(&self, time: String, value: f64, sensor: String) -> Metric {
        Metric::new(
            time,
            "temp".to_string(),
            value,
            self.hardware_info.hostname.clone(),
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

        self.hardware_info.components.refresh(true);
        self.hardware_info.system.refresh_cpu_usage();
        self.hardware_info.system.refresh_memory();
        self.hardware_info.disk.refresh(true);

        let mut disk_total = 0;
        let mut disk_used = 0;
        for disk in self.hardware_info.disk.list() {
            if disk.name().to_str() == Some("/dev/omnect/data") {
                disk_total = disk.total_space();
                disk_used = disk.total_space() - disk.available_space();
                break;
            }
        }

        let mut metric_list = vec![
            self.cpu_usage(time.clone()),
            self.memory_used(time.clone()),
            self.memory_total(time.clone()),
            self.disk_used(time.clone(), disk_used as f64),
            self.disk_total(time.clone(), disk_total as f64),
        ];

        self.hardware_info.components.iter().for_each(|c| {
            if let Some(t) = c.temperature() {
                metric_list.push(self.temp(time.clone(), t.into(), c.label().to_string()))
            };
        });

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

    pub fn bootloader_updated() -> bool {
        Path::new(BOOTLOADER_UPDATED_FILE)
            .try_exists()
            .is_ok_and(|res| res)
    }
}
