use crate::{
    common::RootPartition,
    reboot_reason,
    twin::feature::{self, *},
    web_service,
};
use anyhow::{Context, Result, bail};
use azure_iot_sdk::client::{IotHubClient, IotMessage};
use inotify::WatchMask;
use lazy_static::lazy_static;
use log::{info, warn};
use serde::{Deserialize, Serialize, Serializer, ser::Error};
use serde_json::json;
use std::{
    env,
    path::Path,
    sync::{LazyLock, RwLock},
    time::Duration,
};
use sysinfo;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";

// ToDo
lazy_static! {
    static ref REFRESH_SYSTEM_INFO_INTERVAL_SECS: u64 = {
        const REFRESH_SYSTEM_INFO_INTERVAL_SECS_DEFAULT: &str = "60";
        env::var("REFRESH_SYSTEM_INFO_INTERVAL_SECS")
            .unwrap_or(REFRESH_SYSTEM_INFO_INTERVAL_SECS_DEFAULT.to_string())
            .parse::<u64>()
            .expect("cannot parse REFRESH_SYSTEM_INFO_INTERVAL_SECS env var")
    };
}

#[derive(Default, Serialize)]
struct Label {
    #[serde(serialize_with = "device_id")]
    device_id: (),
    #[serde(serialize_with = "module_name")]
    module_name: (),
    #[serde(skip_serializing_if = "Option::is_none")]
    sensor: Option<String>,
}

fn device_id<S>(_: &(), s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let Some(hostname) = sysinfo::System::host_name() else {
        return Err(Error::custom("failed to get hostname"));
    };

    s.serialize_str(&hostname)
}

fn module_name<S>(_: &(), s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str("omnect-device-service")
}

#[derive(Serialize)]
enum MetricValue {
    CpuUsage(f64),
    MemoryUsed(f64),
    MemoryTotal(f64),
    DiskUsed(f64),
    DiskTotal(f64),
    Temp(f64, String),
}

impl MetricValue {
    fn to_metric(self) -> Metric {
        let (name, value, labels) = match self {
            MetricValue::CpuUsage(value) => ("cpu_usage".to_owned(), value, Label::default()),
            MetricValue::MemoryUsed(value) => ("memory_used".to_owned(), value, Label::default()),
            MetricValue::MemoryTotal(value) => ("memory_total".to_owned(), value, Label::default()),
            MetricValue::DiskUsed(value) => ("disk_used".to_owned(), value, Label::default()),
            MetricValue::DiskTotal(value) => ("disk_total".to_owned(), value, Label::default()),
            MetricValue::Temp(value, sensor) => (
                "temp".to_owned(),
                value,
                Label {
                    sensor: Some(sensor),
                    ..Default::default()
                },
            ),
        };

        Metric {
            metric: InnerMetric {
                name,
                value,
                labels,
            },
            ..Default::default()
        }
    }
}

#[derive(Default, Serialize)]
struct InnerMetric {
    name: String,
    value: f64,
    labels: Label,
}

#[derive(Default, Serialize)]
struct Metric {
    #[serde(serialize_with = "time_stamp")]
    time_generated_utc: (),
    #[serde(flatten)]
    metric: InnerMetric,
}

static TIME_STAMP: LazyLock<RwLock<String>> =
    LazyLock::new(|| RwLock::new(time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap()));

fn time_stamp<S>(_: &(), s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let Ok(time_stamp) = TIME_STAMP.read() else {
        return Err(Error::custom("failed to get timestamp"));
    };

    s.serialize_str(&time_stamp)
}

// ToDo rm lazy_staticw
lazy_static! {
    static ref TIMESYNC_FILE: &'static Path = if cfg!(feature = "mock") {
        Path::new("/tmp/synchronized")
    } else {
        Path::new("/run/systemd/timesync/synchronized")
    };
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
            _ => bail!("unexpected command"),
        }

        Ok(None)
    }
}

impl SystemInfo {
    const SYSTEM_INFO_VERSION: u8 = 1;
    const ID: &'static str = "system_info";

    pub fn new() -> Result<Self> {
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

        feature::add_watch::<Self>(&TIMESYNC_FILE, WatchMask::CREATE | WatchMask::ONESHOT)?;

        if 0 < *REFRESH_SYSTEM_INFO_INTERVAL_SECS {
            feature::notify_interval::<Self>(Duration::from_secs(
                *REFRESH_SYSTEM_INFO_INTERVAL_SECS,
            ))?;
        }

        Ok(SystemInfo {
            tx_reported_properties: None,
            tx_outgoing_message: None,
            software_info: SoftwareInfo {
                os: Self::os_info()?,
                azure_sdk_version,
                omnect_device_service_version,
                boot_time: None,
            },
            hardware_info: HardwareInfo {
                components: sysinfo::Components::new_with_refreshed_list(),
                disk: sysinfo::Disks::new_with_refreshed_list(),
                system: sysinfo::System::new_with_specifics(
                    sysinfo::RefreshKind::nothing()
                        .with_cpu(sysinfo::CpuRefreshKind::everything())
                        .with_memory(sysinfo::MemoryRefreshKind::everything()),
                ),
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

    async fn metrics(&mut self) -> Result<()> {
        let Some(tx) = &self.tx_outgoing_message else {
            warn!("metrics: skip since tx_outgoing_message is None");
            return Ok(());
        };

        let Ok(mut time_stamp) = TIME_STAMP.write() else {
            bail!("metrics: failed to lock TIME_STAMP")
        };
        *time_stamp = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("metrics: failed to get and format timestamp")?;

        self.hardware_info.components.refresh(true);
        self.hardware_info.system.refresh_cpu_usage();
        self.hardware_info.system.refresh_memory();
        self.hardware_info.disk.refresh(true);

        let (disk_total, disk_used) = self
            .hardware_info
            .disk
            .into_iter()
            .filter_map(|disk| match (disk.name().to_str(), disk.total_space()) {
                (Some("/dev/omnect/data"), space) => Some((space, space - disk.available_space())),
                _ => None,
            })
            .next()
            .unwrap_or((0, 0));

        let mut metrics = vec![
            MetricValue::CpuUsage(self.hardware_info.system.global_cpu_usage() as f64).to_metric(),
            MetricValue::MemoryUsed(self.hardware_info.system.used_memory() as f64).to_metric(),
            MetricValue::MemoryTotal(self.hardware_info.system.total_memory() as f64).to_metric(),
            MetricValue::DiskUsed(disk_used as f64).to_metric(),
            MetricValue::DiskTotal(disk_total as f64).to_metric(),
        ];

        self.hardware_info.components.iter().for_each(|c| {
            if let Some(t) = c.temperature() {
                metrics.push(MetricValue::Temp(t.into(), c.label().to_string()).to_metric())
            };
        });

        let json = serde_json::to_vec(&metrics)
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
