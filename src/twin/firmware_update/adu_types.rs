use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub struct UpdateId {
    pub provider: String,
    pub name: String,
    pub version: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserConsentHandlerProperties {
    pub installed_criteria: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SWUpdateHandlerProperties {
    pub installed_criteria: String,
    pub swu_file_name: String,
    pub arguments: String,
    pub script_file_name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
pub enum HandlerProperties {
    UserConsent(UserConsentHandlerProperties),
    SWUpdate(SWUpdateHandlerProperties),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    #[serde(rename = "type")]
    pub step_type: String,
    pub description: String,
    pub handler: String,
    pub files: Vec<String>,
    pub handler_properties: HandlerProperties,
}

#[derive(Serialize, Deserialize)]
pub struct Instructions {
    pub steps: Vec<Step>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub filename: String,
    pub size_in_bytes: u64,
    pub hashes: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
pub struct Compatibility {
    pub manufacturer: String,
    pub model: String,
    pub compatibilityid: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportManifest {
    pub update_id: UpdateId,
    pub is_deployable: bool,
    pub compatibility: Vec<Compatibility>,
    pub instructions: Instructions,
    pub files: Vec<File>,
    pub created_date_time: String,
    pub manifest_version: String,
}

#[derive(Deserialize, Serialize)]
pub struct AdditionalDeviceProperties {
    pub compatibilityid: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub manufacturer: String,
    pub model: String,
    pub additional_device_properties: AdditionalDeviceProperties,
}

#[derive(Deserialize, Serialize)]
pub struct DeviceUpdateConfig {
    pub agents: Vec<Agent>,
}
