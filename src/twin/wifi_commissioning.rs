use super::{Feature, FeatureState};
use std::any::Any;
use std::env;

#[derive(Default)]
pub struct WifiCommissioning {
    state: FeatureState,
}

impl Feature for WifiCommissioning {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::WIFI_COMMISSIONING_VERSION
    }

    fn is_enabled(&self) -> bool {
        if let Ok(value) = env::var("DISTRO_FEATURES") {
            value.contains("wifi-commissioning")
        } else {
            false
        }
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

impl WifiCommissioning {
    const WIFI_COMMISSIONING_VERSION: u8 = 1;
    const ID: &'static str = "wifi";
}
