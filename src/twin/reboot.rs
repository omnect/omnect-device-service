use super::super::systemd;
use super::{Feature, FeatureState};
use crate::twin;
use anyhow::Result;
use futures_executor::block_on;
use log::info;
use std::any::Any;
use std::env;


pub fn reboot() -> Result<Option<serde_json::Value>> {
    twin::get_or_init(None).exec(|twin| {
        twin.feature::<Reboot>()?
            .reboot()
    })
}

#[derive(Default)]
pub struct Reboot {
    state: FeatureState,
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

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn state_mut(&mut self) -> &mut FeatureState {
        &mut self.state
    }

    fn state(&self) -> &FeatureState {
        &self.state
    }
}

impl Reboot {
    const REBOOT_VERSION: u8 = 1;
    const ID: &'static str = "reboot";

    pub fn reboot(&self) -> Result<Option<serde_json::Value>> {
        info!("reboot requested");

        self.ensure()?;

        block_on(async { systemd::reboot().await })?;

        Ok(None)
    }
}
