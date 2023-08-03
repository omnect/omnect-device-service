use super::Feature;
use std::{any::Any, env};

#[derive(Default)]
pub struct WifiCommissioning {}

impl Feature for WifiCommissioning {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::WIFI_COMMISSIONING_VERSION
    }

    fn is_enabled(&self) -> bool {
        if let Ok(value) = env::var("DISTRO_FEATURES") {
            value.split(' ').any(|value| value == "wifi-commissioning")
        } else {
            false
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl WifiCommissioning {
    const WIFI_COMMISSIONING_VERSION: u8 = 1;
    const ID: &'static str = "wifi_commissioning";
}
