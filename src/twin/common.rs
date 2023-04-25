use super::super::systemd;
use super::Twin;
use anyhow::{Context, Result};
use azure_iot_sdk::client::*;
use futures_executor::block_on;
use log::info;
use serde_json::json;

pub fn reboot(_in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("reboot requested");

    block_on(async { systemd::reboot().await })?;

    Ok(None)
}

impl Twin {
    pub fn report_versions(&mut self) -> Result<()> {
        let version = json!({
            "module-version": format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("GIT_SHORT_REV")),
            "azure-sdk-version": IotHubClient::get_sdk_version_string()
        });

        self.report_impl(version)
            .context("report_versions: report_impl")
    }
}
