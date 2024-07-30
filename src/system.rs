use crate::{systemd, systemd::unit::UnitAction};

use anyhow::{bail, Context, Result};
use log::info;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
static DEV_OMNECT: &str = "/dev/omnect/";
static NETWORK_SERVICE: &str = "systemd-networkd.service";
static NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS: u64 = 30;

macro_rules! identity_config_file_path {
    () => {{
        const ENV_FILE_PATH_DEFAULT: &'static str = "/mnt/factory/etc/aziot/config.toml";
        std::env::var("IDENTITY_CONFIG_FILE_PATH").unwrap_or(ENV_FILE_PATH_DEFAULT.to_string())
    }};
}

fn current_root() -> Result<&'static str> {
    let current_root = fs::read_link(DEV_OMNECT.to_owned() + "rootCurrent")
        .context("getting current root device")?;

    if current_root == fs::read_link(DEV_OMNECT.to_owned() + "rootA").context("getting rootA")? {
        return Ok("a");
    }

    if current_root == fs::read_link(DEV_OMNECT.to_owned() + "rootB").context("getting rootB")? {
        return Ok("b");
    }

    bail!("device booted from unknown root")
}

fn bootloader_updated() -> bool {
    Path::new(BOOTLOADER_UPDATED_FILE)
        .try_exists()
        .is_ok_and(|res| res)
}

pub fn infos() -> Result<()> {
    info!("bootloader was updated: {}", bootloader_updated());
    info!("device booted from root {}.", current_root()?);
    Ok(())
}

pub async fn reload_network() -> Result<()> {
    systemd::unit::unit_action(
        NETWORK_SERVICE,
        UnitAction::Reload,
        Duration::from_secs(NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS),
    )
    .await
}

pub fn sw_version() -> Result<serde_json::Value> {
    let path = if cfg!(feature = "mock") {
        "testfiles/positive/sw-versions"
    } else {
        "/etc/sw-versions"
    };

    let sw_versions = std::fs::read_to_string(path).context("cannot read sw-versions")?;
    let sw_versions: Vec<&str> = sw_versions.trim_end().split(' ').collect();

    anyhow::ensure!(
        sw_versions.len() == 2,
        "sw-versions: unexpected number of entries"
    );

    Ok(json!( {
        "osName": sw_versions[0],
        "swVersion": sw_versions[1],
    }))
}

pub fn provisioning_config() -> Result<serde_json::Value> {
    let path = identity_config_file_path!();
    let config: toml::map::Map<String, toml::Value> = std::fs::read_to_string(&path)
        .context(format!("cannot read {}", path))?
        .parse::<toml::Table>()
        .context("cannot parse table")?;
    let prov_source = config["provisioning"]["source"].as_str();
    let prov_auth_method = config["provisioning"]
        .get("authentication")
        .and_then(|val| val.as_str());
    let dps_attestation_method = config["provisioning"]
        .get("attestation")
        .and_then(|val| val.get("method").and_then(|val| val.as_str()));
    let dps_attestation_identity_cert_method =
        config["provisioning"].get("attestation").and_then(|val| {
            val.get("identity_cert")
                .and_then(|val| val.get("method").and_then(|val| val.as_str()))
        });

    match (
        prov_source,
        prov_auth_method,
        dps_attestation_method,
        dps_attestation_identity_cert_method,
    ) {
        (Some("dps"), None, Some("x509"), Some("est")) => Ok(json!(["dps", "x509", "est"])),
        (Some("dps"), None, Some("x509"), None) => Ok(json!(["dps", "x509"])),
        (Some("dps"), None, Some("tpm"), None) => Ok(json!(["dps", "tpm"])),
        (Some("dps"), None, Some("symmetric_key"), None) => Ok(json!(["dps", "symmetric_key"])),
        (Some("manual"), Some("sas"), None, None) => Ok(json!(["manual", "sas"])),
        (Some("manual"), Some("x509"), None, None) => Ok(json!(["manual", "x509"])),
        _ => bail!("invalid provisioning configuration found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_config_test() {
        std::env::set_var("IDENTITY_CONFIG_FILE_PATH", "");
        assert!(provisioning_config()
            .unwrap_err()
            .to_string()
            .starts_with("cannot read"));

        std::env::set_var(
            "IDENTITY_CONFIG_FILE_PATH",
            "testfiles/positive/config.toml.est",
        );
        assert_eq!(
            provisioning_config().unwrap(),
            json!(["dps", "x509", "est"])
        );
    }
}
