use super::util;
use super::Feature;
use crate::twin::TypeIdStream;
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use futures::StreamExt;
use lazy_static::lazy_static;
use log::{debug, info, warn};
use serde::Serialize;
use serde_json::json;
use std::{any::Any, env, path::Path, time::Duration};
use time::format_description::well_known::Rfc3339;
use tokio::{sync::mpsc::Sender, time::interval};

lazy_static! {
    static ref REFRESH_EST_EXPIRY_INTERVAL_SECS: u64 = {
        const REFRESH_EST_EXPIRY_INTERVAL_SECS_DEFAULT: &str = "180";
        std::env::var("REFRESH_EST_EXPIRY_INTERVAL_SECS")
            .unwrap_or(REFRESH_EST_EXPIRY_INTERVAL_SECS_DEFAULT.to_string())
            .parse::<u64>()
            .expect("cannot parse REFRESH_EST_EXPIRY_INTERVAL_SECS env var")
    };
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Dps,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct X509 {
    est: bool,
    expires: String,
}

impl X509 {
    fn new(est: bool, hostname: &String) -> Result<Self> {
        // currently we expect cert paths depending on using EST or not.
        // maybe we should change to "/var/lib/aziot/certd/certs/" for all scenarios?
        let glob_path = if est {
            const EST_CERT_FILE_PATH_DEFAULT: &str = "/var/lib/aziot/certd/certs/deviceid-*.cer";
            env::var("EST_CERT_FILE_PATH").unwrap_or(EST_CERT_FILE_PATH_DEFAULT.to_string())
        } else {
            const DEVICE_CERT_FILE_PATH_DEFAULT: &str = "/mnt/cert/priv/device_id_cert.pem";
            env::var("DEVICE_CERT_FILE_PATH").unwrap_or(DEVICE_CERT_FILE_PATH_DEFAULT.to_string())
        };

        let paths: Vec<std::path::PathBuf> = glob::glob(&glob_path)
            .context(format!(
                "provisioning_config: cannot read glob pattern {glob_path}"
            ))?
            .filter_map(Result::ok)
            .collect();

        let expires = match paths.len() {
            0 => {
                if est {
                    warn!("no certs found. maybe device isn't provisioned yet?");
                    "unknown".to_owned()
                } else {
                    bail!("couldn't find certificate")
                }
            }
            1 => Self::expires(paths[0].as_path(), hostname)?,
            num => bail!("expected exactly 1 certificate but found {num}"),
        };

        Ok(X509 { est, expires })
    }

    fn expires(cert_path: &Path, hostname: &String) -> Result<String> {
        let file = std::io::BufReader::new(
            std::fs::File::open(cert_path)
                .context(format!("provisioning_config: cannot read {:?}", cert_path))?,
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

        cert.validity()
            .not_after
            .to_datetime()
            .format(&Rfc3339)
            .context("provisioning_config: format date")
    }
}

#[derive(Clone, Debug, Serialize)]
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

    fn refresh_event(&mut self) -> Result<Option<TypeIdStream>> {
        if !self.is_enabled() || 0 == *REFRESH_EST_EXPIRY_INTERVAL_SECS {
            return Ok(None);
        }

        match &self.method {
            Method::X509(cert) if cert.est => Ok(Some(
                util::IntervalStreamTypeId::new(
                    interval(Duration::from_secs(*REFRESH_EST_EXPIRY_INTERVAL_SECS)),
                    Self::type_id(self),
                )
                .boxed(),
            )),
            _ => Ok(None),
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        self.ensure()?;

        let expires = match &self.method {
            Method::X509(X509 { est, expires, .. }) if *est => expires,
            _ => bail!("refresh: unexpected provisioning method"),
        };

        let x509 = X509::new(true, &self.hostname)?;

        if &x509.expires != expires {
            info!("refresh: est expiration date changed {}", &x509.expires);

            self.method = Method::X509(x509);
            self.report().await?;
        } else {
            debug!("refresh: est expiration date didn't change");
        }

        Ok(())
    }
}

impl ProvisioningConfig {
    const PROVISIONING_CONFIG_VERSION: u8 = 1;
    const ID: &'static str = "provisioning_config";
    const IDENTITY_CONFIG_FILE_PATH_DEFAULT: &'static str = "/etc/aziot/config.toml";

    pub fn new() -> Result<Self> {
        let path = env::var("IDENTITY_CONFIG_FILE_PATH")
            .unwrap_or(ProvisioningConfig::IDENTITY_CONFIG_FILE_PATH_DEFAULT.to_string());
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

        env::set_var("EST_CERT_FILE_PATH", "testfiles/positive/deviceid1-*.cer");

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

        let Method::X509(est1) = config.method.clone() else {
            panic!("no X509 found");
        };

        env::set_var("EST_CERT_FILE_PATH", "testfiles/positive/deviceid2-*.cer");

        config.refresh().await.unwrap();
        let Method::X509(est2) = config.method.clone() else {
            panic!("no X509 found");
        };
        assert!(est1 != est2);
    }
}
