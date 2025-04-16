use crate::{
    reboot_reason, systemd,
    twin::{feature::*, Feature},
    web_service,
};
use anyhow::{bail, Context, Result};
use azure_iot_sdk::client::IotMessage;
use log::{debug, error, info};
use serde::Deserialize;
use serde_json::json;
use std::{env, time::Duration};
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SetWaitOnlineTimeoutCommand {
    pub timeout_secs: Option<u64>,
}

#[derive(Default)]
pub struct Reboot {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
}

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
        self.tx_reported_properties = Some(tx_reported_properties);

        self.report_wait_online_timeout().await
    }

    async fn connect_web_service(&self) -> Result<()> {
        self.report_wait_online_timeout().await
    }

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        match cmd {
            Command::Reboot => self.reboot().await,
            Command::SetWaitOnlineTimeout(cmd) => self.set_wait_online_timeout(cmd).await,
            _ => bail!("unexpected command"),
        }
    }
}

impl Reboot {
    const REBOOT_VERSION: u8 = 2;
    const ID: &'static str = "reboot";

    async fn reboot(&self) -> CommandResult {
        info!("reboot requested");

        if let Err(e) =
            reboot_reason::write_reboot_reason("ods-reboot", "initiated by portal or API")
        {
            error!(": failed to write reboot reason [{e}]");
        }

        systemd::reboot().await?;

        Ok(None)
    }

    async fn set_wait_online_timeout(&self, cmd: &SetWaitOnlineTimeoutCommand) -> CommandResult {
        info!("set wait_online_timeout requested: {cmd:?}");

        systemd::networkd::set_networkd_wait_online_timeout(
            cmd.timeout_secs.map(Duration::from_secs),
        )?;

        self.report_wait_online_timeout().await?;

        Ok(None)
    }

    async fn report_wait_online_timeout(&self) -> Result<()> {
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
        .await;

        Ok(())
    }
}
