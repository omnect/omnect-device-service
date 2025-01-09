use super::Feature;
use std::env;

#[derive(Default)]
pub struct WifiCommissioning {}

impl Feature for WifiCommissioning {
    async fn name(&self) -> String {
        Self::ID.to_string()
    }

    async fn version(&self) -> u8 {
        Self::WIFI_COMMISSIONING_VERSION
    }

    async fn is_enabled(&self) -> bool {
        if let Ok(value) = env::var("DISTRO_FEATURES") {
            value.split(' ').any(|value| value == "wifi-commissioning")
        } else {
            false
        }
    }
}

impl WifiCommissioning {
    const WIFI_COMMISSIONING_VERSION: u8 = 1;
    const ID: &'static str = "wifi_commissioning";
}
