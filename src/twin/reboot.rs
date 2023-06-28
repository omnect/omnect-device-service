use super::super::systemd;
use super::Feature;
use anyhow::Result;
use log::info;
use std::{any::Any, env};

#[derive(Default)]
pub struct Reboot {}

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
}

impl Reboot {
    const REBOOT_VERSION: u8 = 1;
    const ID: &'static str = "reboot";

    pub async fn reboot(&self) -> Result<Option<serde_json::Value>> {
        info!("reboot requested");

        self.ensure()?;

        systemd::reboot().await?;

        Ok(None)
    }
}
