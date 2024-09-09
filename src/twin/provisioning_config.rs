use super::Feature;
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use futures::future;
use log::{debug, info, warn};
use notify::{Config, INotifyWatcher, RecommendedWatcher, Watcher};
use serde::Serialize;
use serde_json::json;
use std::{any::Any, cell::RefCell, env, path::PathBuf};
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc::{channel, Receiver, Sender};

macro_rules! identity_config_file_path {
    () => {{
        const ENV_FILE_PATH_DEFAULT: &'static str = "/etc/aziot/config.toml";
        env::var("IDENTITY_CONFIG_FILE_PATH").unwrap_or(ENV_FILE_PATH_DEFAULT.to_string())
    }};
}

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
    #[serde(skip_serializing)]
    path: PathBuf,
}

impl X509 {
    fn new(est: bool) -> Result<Self> {
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
        let expires = x509_parser::pem::Pem::read(file)
            .context("provisioning_config: read PEM")?
            .0
            .parse_x509()
            .context("provisioning_config: parse x509")?
            .tbs_certificate
            .validity()
            .not_after
            .to_datetime()
            .format(&Rfc3339)
            .context("provisioning_config: format date")?;

        Ok(X509 {
            est,
            expires,
            path: paths[0].to_path_buf(),
        })
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

type WatcherReceiver = Receiver<notify::Result<notify::Event>>;

#[derive(Debug, Serialize)]
pub struct ProvisioningConfig {
    #[serde(skip_serializing)]
    identity_watcher: Option<RecommendedWatcher>,
    #[serde(skip_serializing)]
    rx_identity_watcher: Option<RefCell<WatcherReceiver>>,
    #[serde(skip_serializing)]
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    source: Source,
    method: Method,
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
        let (source, method) = Self::config()?;
        let (identity_watcher, rx_identity_watcher) = Self::watcher(&method)?;
        let this = ProvisioningConfig {
            identity_watcher,
            rx_identity_watcher,
            tx_reported_properties: None,
            source,
            method,
        };

        debug!("provisioning_config: new {this:?}");

        Ok(this)
    }

    /*
    reading https://rust-lang.github.io/rust-clippy/master/index.html#await_holding_refcell_ref
    there should not be an issue here. thus we suppress clippy warning.
    */
    #[allow(clippy::await_holding_refcell_ref)]
    pub async fn next(&self) {
        let Some(ref rx) = self.rx_identity_watcher else {
            debug!("provisioning_config watcher: not active");
            return future::pending().await;
        };

        let Some(_) = rx.borrow_mut().recv().await else {
            debug!("provisioning_config watcher: sender dropped");
            return future::pending().await;
        };

        info!("provisioning_config watcher: config changed");

        future::ready(()).await
    }

    pub async fn refresh(&mut self) -> Result<()> {
        info!("provisioning_config: refresh");

        self.ensure()?;

        let (source, method) = Self::config()?;
        let (identity_watcher, rx_identity_watcher) = Self::watcher(&method)?;

        self.source = source;
        self.method = method;
        self.identity_watcher = identity_watcher;
        self.rx_identity_watcher = rx_identity_watcher;

        debug!("provisioning_config: refresh {self:?}");

        self.report().await
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

    #[allow(unreachable_code, unused_variables)]
    fn watcher(
        method: &Method,
    ) -> Result<(Option<INotifyWatcher>, Option<RefCell<WatcherReceiver>>)> {
        /*
            we deactivate observing the certificate for the moment, since it is not completely tested yet
        */
        return Ok((None, None));

        let mut watcher = None;
        let mut receiver = None;

        if matches!(method, Method::X509(_)) {
            let Method::X509(cert) = method else {
                bail!("provisioning_config watcher: unexpected certificate")
            };
            let (tx, rx) = channel(1);
            let mut w = RecommendedWatcher::new(
                move |res| {
                    futures::executor::block_on(async {
                        tx.send(res).await.unwrap();
                    })
                },
                Config::default(),
            )?;

            w.watch(cert.path.as_path(), notify::RecursiveMode::NonRecursive)?;

            watcher = Some(w);
            receiver = Some(RefCell::new(rx));
        }

        Ok((watcher, receiver))
    }

    fn config() -> Result<(Source, Method)> {
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

        let method = match (prov_auth_method, dps_attestation_method) {
            (Some(method), None) => method,
            (None, Some(method)) => method,
            _ => bail!("provisioning_config: invalid provisioning method found"),
        };

        match (prov_source, method, dps_attestation_identity_cert_method) {
            (Some("dps"), "x509", Some("est")) => Ok((Source::Dps, Method::X509(X509::new(true)?))),
            //fix
            (Some("dps"), "x509", None) => Ok((Source::Dps, Method::X509(X509::new(false)?))),
            (Some("dps"), "tpm", None) => Ok((Source::Dps, Method::Tpm)),
            (Some("dps"), "symmetric_key", None) => Ok((Source::Dps, Method::SymetricKey)),
            (Some("manual"), "sas", None) => Ok((Source::Manual, Method::Sas)),
            //fix
            (Some("manual"), "x509", None) => Ok((Source::Manual, Method::X509(X509::new(false)?))),
            _ => bail!("provisioning_config: invalid provisioning configuration found"),
        }
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
        env::set_var("EST_CERT_FILE_PATH", "testfiles/positive/deviceid-*.cer");
        assert_eq!(
            serde_json::to_value(ProvisioningConfig::new().unwrap()).unwrap(),
            json!({
                "source": "dps",
                "method": {
                    "x509": {
                        "expires": "2024-06-21T07:12:30Z",
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
}
