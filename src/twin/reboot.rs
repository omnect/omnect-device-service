use crate::web_service;

use super::super::systemd;
use super::Feature;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{debug, info};
use serde_json::json;
use std::{any::Any, env, time::Duration};
use tokio::sync::mpsc::Sender;

#[derive(Default)]
pub struct Reboot {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
}

#[async_trait(?Send)]
impl Feature for Reboot {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::REBOOT_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_REBOOT") != Ok("true".to_string())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.ensure()?;

        self.tx_reported_properties = Some(tx_reported_properties);

        self.report_wait_online_timeout().await
    }

    async fn connect_web_service(&self) -> Result<()> {
        self.ensure()?;

        self.report_wait_online_timeout().await
    }
}

impl Reboot {
    const REBOOT_VERSION: u8 = 2;
    const ID: &'static str = "reboot";

    pub async fn reboot(&self) -> Result<Option<serde_json::Value>> {
        info!("reboot requested");

        self.ensure()?;

        systemd::reboot().await?;

        Ok(None)
    }

    pub async fn set_wait_online_timeout(
        &self,
        in_json: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("set wait_online_timeout requested: {in_json}");

        self.ensure()?;

        match &in_json["timeout_secs"] {
            serde_json::Value::Number(timeout) if timeout.is_u64() => {
                systemd::wait_online::set_networkd_wait_online_timeout(Some(Duration::from_secs(
                    timeout.as_u64().unwrap(),
                )))?
            }
            serde_json::Value::Null => {
                systemd::wait_online::set_networkd_wait_online_timeout(None)?
            }
            value => bail!("invalid value for timeout_secs: {value}"),
        }

        self.report_wait_online_timeout().await?;

        Ok(None)
    }

    async fn report_wait_online_timeout(&self) -> Result<()> {
        self.ensure()?;

        let timeout = systemd::wait_online::networkd_wait_online_timeout()?;

        if let Some(tx) = &self.tx_reported_properties {
            tx.send(json!({
                "wait_online_timeout_secs": timeout
            }))
            .await
            .context("report_wait_online_timeout: send")?;
        } else {
            debug!("report_wait_online_timeout: skip since tx_reported_properties is None");
        };

        web_service::publish(
            web_service::PublishChannel::Timeouts,
            json!({
                "wait-online-timeout": timeout
            }),
        )
        .await
        .context("report_wait_online_timeout: publish")?;

        Ok(())
    }
}
