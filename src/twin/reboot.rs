use super::super::systemd;
use super::web_service;
use super::{feature, Feature};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{debug, info};
use serde::Deserialize;
use serde_json::json;
use std::{env, time::Duration};
use tokio::sync::mpsc::Sender;

#[derive(Debug, Deserialize)]
pub(crate) struct SetWaitOnlineTimeoutCommand {
    timeout_secs: Option<u64>,
}

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

    async fn command(&mut self, cmd: feature::Command) -> Result<Option<serde_json::Value>> {
        self.ensure()?;

        match cmd {
            feature::Command::Reboot => self.reboot().await,
            feature::Command::SetWaitOnlineTimeout(cmd) => self.set_wait_online_timeout(cmd).await,
            _ => bail!("unexpected command"),
        }
    }
}

impl Reboot {
    const REBOOT_VERSION: u8 = 2;
    const ID: &'static str = "reboot";

    async fn reboot(&self) -> Result<Option<serde_json::Value>> {
        info!("reboot requested");

        self.ensure()?;

        systemd::reboot().await?;

        Ok(None)
    }

    async fn set_wait_online_timeout(
        &self,
        cmd: SetWaitOnlineTimeoutCommand,
    ) -> Result<Option<serde_json::Value>> {
        info!("set wait_online_timeout requested: {cmd:?}");

        self.ensure()?;

        systemd::networkd::set_networkd_wait_online_timeout(
            cmd.timeout_secs.map(Duration::from_secs),
        )?;

        self.report_wait_online_timeout().await?;

        Ok(None)
    }

    async fn report_wait_online_timeout(&self) -> Result<()> {
        self.ensure()?;

        let timeout = systemd::networkd::networkd_wait_online_timeout()?;

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
                "wait_online_timeout": timeout
            }),
        )
        .await
        .context("report_wait_online_timeout: publish")?;

        Ok(())
    }
}
