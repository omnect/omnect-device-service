use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub(super) struct UpdateId {
    pub(super) provider: String,
    pub(super) name: String,
    pub(super) version: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserConsentHandlerProperties {
    pub(super) installed_criteria: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SWUpdateHandlerProperties {
    pub(super) installed_criteria: String,
    pub(super) swu_file_name: String,
    pub(super) arguments: String,
    pub(super) script_file_name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
pub(super) enum HandlerProperties {
    UserConsent(UserConsentHandlerProperties),
    SWUpdate(SWUpdateHandlerProperties),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Step {
    #[serde(rename = "type")]
    pub(super) step_type: String,
    pub(super) description: String,
    pub(super) handler: String,
    pub(super) files: Vec<String>,
    pub(super) handler_properties: HandlerProperties,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Instructions {
    pub(super) steps: Vec<Step>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct File {
    pub(super) filename: String,
    pub(super) size_in_bytes: u64,
    pub(super) hashes: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Compatibility {
    pub(super) manufacturer: String,
    pub(super) model: String,
    pub(super) compatibilityid: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImportManifest {
    pub(super) update_id: UpdateId,
    pub(super) is_deployable: bool,
    pub(super) compatibility: Vec<Compatibility>,
    pub(super) instructions: Instructions,
    pub(super) files: Vec<File>,
    pub(super) created_date_time: String,
    pub(super) manifest_version: String,
}

#[derive(Deserialize)]
pub(super) struct AdditionalDeviceProperties {
    pub(super) compatibilityid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Agent {
    pub(super) manufacturer: String,
    pub(super) model: String,
    pub(super) additional_device_properties: AdditionalDeviceProperties,
}

#[derive(Deserialize)]
pub(super) struct DeviceUpdateConfig {
    pub(super) agents: Vec<Agent>,
}
