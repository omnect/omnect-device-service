use crate::{systemd, systemd::unit::UnitAction};

use anyhow::{bail, ensure, Context, Result};
use log::info;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
static DEV_OMNECT: &str = "/dev/omnect/";
static NETWORK_SERVICE: &str = "systemd-networkd.service";
static NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS: u64 = 30;

macro_rules! identity_config_file_path {
    () => {{
        const ENV_FILE_PATH_DEFAULT: &'static str = "/etc/aziot/config.toml";
        std::env::var("IDENTITY_CONFIG_FILE_PATH").unwrap_or(ENV_FILE_PATH_DEFAULT.to_string())
    }};
}

macro_rules! device_cert_file_path {
    () => {{
        const ENV_FILE_PATH_DEFAULT: &'static str = "/var/lib/aziot/certd/certs/deviceid-*.cer";
        std::env::var("DEVICE_CERT_FILE_PATH").unwrap_or(ENV_FILE_PATH_DEFAULT.to_string())
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
        .context(format!("provisioning_config: cannot read {path}"))?
        .parse::<toml::Table>()
        .context("provisioning_config: cannot parse table")?;
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

    let not_after = if prov_auth_method.is_some_and(|val| val == "x509")
        || dps_attestation_method.is_some_and(|val| val == "x509")
    {
        let glob_path = device_cert_file_path!();
        let paths: Vec<std::path::PathBuf> = glob::glob(&glob_path)
            .context(format!(
                "provisioning_config: cannot read glob pattern {glob_path}"
            ))?
            .filter_map(Result::ok)
            .collect();

        let num_certs = paths.len();

        ensure!(
        num_certs == 1,
        "provisioning_config: found {num_certs} certs instead of one with glob path {glob_path}."
    );

        let file = std::io::BufReader::new(
            std::fs::File::open(&paths[0])
                .context(format!("provisioning_config: cannot read {:?}", paths[0]))?,
        );
        x509_parser::pem::Pem::read(file)
            .context("provisioning_config: read PEM")?
            .0
            .parse_x509()
            .context("provisioning_config: parse x509")?
            .tbs_certificate
            .validity()
            .not_after
            .to_datetime()
            .format(&Rfc3339)
            .context("provisioning_config: format date")?
    } else {
        "".to_owned()
    };

    match (
        prov_source,
        prov_auth_method,
        dps_attestation_method,
        dps_attestation_identity_cert_method,
    ) {
        (Some("dps"), None, Some("x509"), Some("est")) => Ok(json!({
            "source": "dps",
            "method": "x509",
            "x509": {
                "expires": not_after,
                "est": true
            }
        })),
        (Some("dps"), None, Some("x509"), None) => Ok(json!({
            "source": "dps",
            "method": "x509",
            "x509": {
                "expires": not_after,
                "est": false
            }
        })),
        (Some("dps"), None, Some("tpm"), None) => Ok(json!({
            "source": "dps",
            "method": "tpm"
        })),
        (Some("dps"), None, Some("symmetric_key"), None) => Ok(json!({
            "source": "dps",
            "method": "symmetric_key"
        })),
        (Some("manual"), Some("sas"), None, None) => Ok(json!({
            "source": "manual",
            "method": "sas"
        })),
        (Some("manual"), Some("x509"), None, None) => Ok(json!({
            "source": "manual",
            "method": "x509",
            "x509": {
                "expires": not_after,
                "est": false
            }
        })),
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
            .starts_with("provisioning_config: cannot read"));

        std::env::set_var(
            "IDENTITY_CONFIG_FILE_PATH",
            "testfiles/positive/config.toml.est",
        );
        std::env::set_var("DEVICE_CERT_FILE_PATH", "testfiles/positive/deviceid-*.cer");
        assert_eq!(
            provisioning_config().unwrap(),
            json!({
                "source": "dps",
                "method": "x509",
                "x509": {
                    "expires": "2024-06-21T07:12:30Z",
                    "est": true
                }
            })
        );

        std::env::set_var(
            "IDENTITY_CONFIG_FILE_PATH",
            "testfiles/positive/config.toml.tpm",
        );
        assert_eq!(
            provisioning_config().unwrap(),
            json!({
                "source": "dps",
                "method": "tpm"
            })
        );
    }
}
