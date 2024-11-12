use super::web_service;
use super::{feature::*, Feature};
use crate::system;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::{IotHubClient, IotMessage};
use lazy_static::lazy_static;
use log::{debug, warn};
use serde::Serialize;
use serde_json::json;
use std::env;
use std::path::Path;
use tokio::sync::mpsc;

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
        _tx_outgoing_message: mpsc::Sender<IotMessage>,
    ) -> Result<()> {
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
}
