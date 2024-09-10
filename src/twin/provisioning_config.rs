use super::Feature;
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{debug, info, warn};
use serde::Serialize;
use serde_json::json;
use std::{any::Any, env, time::Duration};
use time::format_description::well_known::Rfc3339;
use tokio::{
    sync::mpsc::Sender,
    time::{interval, Interval},
};

macro_rules! identity_config_file_path {
    () => {{
        const ENV_FILE_PATH_DEFAULT: &'static str = "/etc/aziot/config.toml";
        env::var("IDENTITY_CONFIG_FILE_PATH").unwrap_or(ENV_FILE_PATH_DEFAULT.to_string())
    }};
}

// currently we expect certs depending on EST on/off
// maybe we should change to "/var/lib/aziot/certd/certs/" for both?
macro_rules! cert_file_path {
    ($val:expr) => {
        match $val {
            true => {
                const EST_CERT_FILE_PATH_DEFAULT: &'static str =
                    "/var/lib/aziot/certd/certs/deviceid-*.cer";
                env::var("EST_CERT_FILE_PATH").unwrap_or(EST_CERT_FILE_PATH_DEFAULT.to_string())
            }
            false => {
                const DEVICE_CERT_FILE_PATH_DEFAULT: &'static str =
                    "/mnt/cert/priv/device_id_cert.pem";
                env::var("DEVICE_CERT_FILE_PATH")
                    .unwrap_or(DEVICE_CERT_FILE_PATH_DEFAULT.to_string())
            }
        }
    };
}

macro_rules! refresh_est_expiry_interval_secs {
    () => {{
        static REFRESH_EST_EXPIRY_INTERVAL_SECS_DEFAULT: &'static str = "60";
        std::env::var("REFRESH_EST_EXPIRY_INTERVAL_SECS")
            .unwrap_or(REFRESH_EST_EXPIRY_INTERVAL_SECS_DEFAULT.to_string())
            .parse::<u64>()
            .expect("cannot parse REFRESH_EST_EXPIRY_INTERVAL_SECS env var")
    }};
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Dps,
    Manual,
}

#[derive(Debug, Serialize)]
struct X509 {
    est: bool,
    expires: String,
}

impl X509 {
    fn new(est: bool, hostname: &String) -> Result<Self> {
        let glob_path = cert_file_path!(est);

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

        let pem = x509_parser::pem::Pem::read(file).context("provisioning_config: read PEM")?;

        let cert = pem
            .0
            .parse_x509()
            .context("provisioning_config: parse x509")?
            .tbs_certificate;

        ensure!(
            cert.subject.iter_common_name().count() == 1,
            "provisioning_config: unexpected cname count"
        );

        let cname = cert.subject.iter_common_name().next().unwrap().as_str()?;
        ensure!(
            hostname == cname,
            "provisioning_config: hostname and cname don't match ({hostname} vs {cname})"
        );

        let expires = cert
            .validity()
            .not_after
            .to_datetime()
            .format(&Rfc3339)
            .context("provisioning_config: format date")?;

        Ok(X509 { est, expires })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    Sas,
    SymetricKey,
    Tpm,
    X509(X509),
}

#[derive(Debug, Serialize)]
pub struct ProvisioningConfig {
    #[serde(skip_serializing)]
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    source: Source,
    method: Method,
    #[serde(skip_serializing)]
    hostname: String,
}

#[async_trait(?Send)]
impl Feature for ProvisioningConfig {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::PROVISIONING_CONFIG_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_PROVISIONING_CONFIG") != Ok("true".to_string())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.ensure()?;

        self.tx_reported_properties = Some(tx_reported_properties.clone());

        self.report().await
    }
}

impl ProvisioningConfig {
    const PROVISIONING_CONFIG_VERSION: u8 = 1;
    const ID: &'static str = "provisioning_config";

    pub fn new() -> Result<Self> {
        let path = identity_config_file_path!();
        let config: toml::map::Map<String, toml::Value> = std::fs::read_to_string(&path)
            .context(format!("provisioning_config: cannot read {path}"))?
            .parse::<toml::Table>()
            .context("provisioning_config: cannot parse table")?;
        let hostname = config
            .get("hostname")
            .context("provisioning_config: hostname not found")?
            .as_str()
            .context("provisioning_config: hostname cannot be parsed")?
            .to_string();
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

        let method = match (prov_auth_method, dps_attestation_method) {
            (Some(method), None) => method,
            (None, Some(method)) => method,
            _ => bail!("provisioning_config: invalid provisioning method found"),
        };

        let (source, method) = match (prov_source, method, dps_attestation_identity_cert_method) {
            (Some("dps"), "x509", Some("est")) => {
                (Source::Dps, Method::X509(X509::new(true, &hostname)?))
            }
            (Some("dps"), "x509", None) => {
                (Source::Dps, Method::X509(X509::new(false, &hostname)?))
            }
            (Some("dps"), "tpm", None) => (Source::Dps, Method::Tpm),
            (Some("dps"), "symmetric_key", None) => (Source::Dps, Method::SymetricKey),
            (Some("manual"), "sas", None) => (Source::Manual, Method::Sas),
            (Some("manual"), "x509", None) => {
                (Source::Manual, Method::X509(X509::new(false, &hostname)?))
            }
            _ => bail!("provisioning_config: invalid provisioning configuration found"),
        };

        let this = ProvisioningConfig {
            tx_reported_properties: None,
            source,
            method,
            hostname,
        };

        debug!("provisioning_config: new {this:?}");

        Ok(this)
    }

    pub fn refresh_interval(&self) -> Option<Interval> {
        match &self.method {
            Method::X509(cert) if cert.est => Some(interval(Duration::from_secs(
                refresh_est_expiry_interval_secs!(),
            ))),
            _ => None,
        }
    }

    pub async fn refresh(&mut self) -> Result<bool> {
        debug!("refresh: read est certificate expiration date");

        self.ensure()?;

        let expires = match &self.method {
            Method::X509(X509 { est, expires, .. }) if *est => expires,
            _ => bail!("refresh: unexpected method"),
        };

        let x509 = X509::new(true, &self.hostname)?;

        if &x509.expires != expires {
            info!("refresh: expiration date changed {}", &x509.expires);

            self.method = Method::X509(x509);
            self.report().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn report(&self) -> Result<()> {
        let Some(tx) = &self.tx_reported_properties else {
            warn!("report: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({
            "provisioning_config": serde_json::to_value(self).context("report: cannot serialize")?
        }))
        .await
        .context("report: send")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_config_test() {
        env::set_var("IDENTITY_CONFIG_FILE_PATH", "");
        assert!(ProvisioningConfig::new()
            .unwrap_err()
            .to_string()
            .starts_with("provisioning_config: cannot read"));

        env::set_var(
            "IDENTITY_CONFIG_FILE_PATH",
            "testfiles/positive/config.toml.est",
        );
        env::set_var("EST_CERT_FILE_PATH", "testfiles/positive/deviceid1-*.cer");
        assert_eq!(
            serde_json::to_value(ProvisioningConfig::new().unwrap()).unwrap(),
            json!({
                "source": "dps",
                "method": {
                    "x509": {
                        "expires": "2024-10-10T11:27:38Z",
                        "est": true,
                    }
                }
            })
        );

        env::set_var(
            "IDENTITY_CONFIG_FILE_PATH",
            "testfiles/positive/config.toml.tpm",
        );
        assert_eq!(
            serde_json::to_value(ProvisioningConfig::new().unwrap()).unwrap(),
            json!({
                "source": "dps",
                "method": "tpm"
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn provisioning_refresh_test() {
        env::set_var(
            "IDENTITY_CONFIG_FILE_PATH",
            "testfiles/positive/config.toml.est",
        );

        env::set_var(
            "EST_CERT_FILE_PATH",
            "testfiles/positive/deviceid1-*.cer",
        );

        let mut config = ProvisioningConfig::new().unwrap();

        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            json!({
                "source": "dps",
                "method": {
                    "x509": {
                        "expires": "2024-10-10T11:27:38Z",
                        "est": true,
                    }
                }
            })
        );

        assert_eq!(config.refresh().await.unwrap(), false);

        env::set_var(
            "EST_CERT_FILE_PATH",
            "testfiles/positive/deviceid2-*.cer",
        );

        assert_eq!(config.refresh().await.unwrap(), true);
    }
}
