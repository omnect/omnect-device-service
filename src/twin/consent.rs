use super::Feature;
use crate::consent_path;
use anyhow::{anyhow, ensure, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use futures_executor::block_on;
use log::{debug, error, info};
use notify::{INotifyWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind, Debouncer};
use serde_json::json;
use std::{
    any::Any,
    env,
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc::Sender;

#[macro_export]
macro_rules! consent_path {
    () => {{
        static CONSENT_DIR_PATH_DEFAULT: &'static str = "/etc/omnect/consent";
        std::env::var("CONSENT_DIR_PATH").unwrap_or(CONSENT_DIR_PATH_DEFAULT.to_string())
    }};
}

macro_rules! request_consent_path {
    () => {{
        PathBuf::from(&format!(r"{}/request_consent.json", consent_path!()))
    }};
}

#[macro_export]
macro_rules! history_consent_path {
    () => {{
        PathBuf::from(&format!(r"{}/history_consent.json", consent_path!()))
    }};
}

#[derive(Default)]
pub struct DeviceUpdateConsent {
    file_observer: Option<Debouncer<INotifyWatcher>>,
    tx_reported_properties: Option<Sender<serde_json::Value>>,
}

#[async_trait(?Send)]
impl Feature for DeviceUpdateConsent {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::USER_CONSENT_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_DEVICE_UPDATE_USER_CONSENT") != Ok("true".to_string())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.ensure()?;

        self.tx_reported_properties = Some(tx_reported_properties.clone());

        Self::report_general_consent(&tx_reported_properties).await?;
        Self::report_user_consent(&tx_reported_properties, &request_consent_path!()).await?;
        Self::report_user_consent(&tx_reported_properties, &history_consent_path!()).await?;

        self.observe_consent()
    }
}

impl DeviceUpdateConsent {
    const USER_CONSENT_VERSION: u8 = 1;
    const ID: &'static str = "device_update_consent";
    const FILE_WATCHER_DELAY_SECS: u64 = 1;

    pub fn user_consent(&self, in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
        info!("user consent requested: {in_json}");

        self.ensure()?;

        match serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(in_json) {
            Ok(map) if map.len() == 1 && map.values().next().unwrap().is_string() => {
                let (component, version) = map.iter().next().unwrap();
                ensure!(
                    !component.contains(std::path::is_separator),
                    "user_consent: invalid component name: {component}"
                );

                let path_string = format!("{}/{}/user_consent.json", consent_path!(), component);
                let path = Path::new(&path_string);
                let path_canonicalized = path.canonicalize().context(format!(
                    "user_consent: invalid path {}",
                    path.to_string_lossy()
                ))?;

                // ensure path is valid and absolute and not a link
                ensure!(
                    path_canonicalized
                        .to_string_lossy()
                        .eq(path_string.as_str()),
                    "user_consent: non-absolute path {}",
                    path.to_string_lossy()
                );

                serde_json::to_writer_pretty(
                    OpenOptions::new()
                        .write(true)
                        .create(false)
                        .truncate(true)
                        .open(path)
                        .context("user_consent: open user_consent.json for write")?,
                    &json!({ "consent": version }),
                )
                .context("user_consent: serde_json::to_writer_pretty")?;

                Ok(None)
            }
            _ => anyhow::bail!("user_consent: unexpected parameter format"),
        }
    }

    fn observe_consent(&mut self) -> Result<()> {
        let Some(tx) = &self.tx_reported_properties else {
            anyhow::bail!("observe_consent: tx_reported_properties is None")
        };

        let tx = tx.clone();

        let mut debouncer = new_debouncer(
            Duration::from_secs(Self::FILE_WATCHER_DELAY_SECS),
            move |res: DebounceEventResult| match res {
                Ok(events) => events.iter().for_each(|ev| {
                    debug!("observe_consent: event: {ev:#?}");

                    if ev.kind == DebouncedEventKind::Any {
                        block_on(async { Self::report_user_consent(&tx, &ev.path).await })
                            .unwrap_or_else(|e| {
                                error!("observe_consent: twin report user consent: {e:#}")
                            });
                    }
                }),
                Err(errors) => errors
                    .paths
                    .iter()
                    .for_each(|e| error!("observe_consent: {e:?}")),
            },
        )?;

        debouncer
            .watcher()
            .watch(
                request_consent_path!().as_path(),
                RecursiveMode::NonRecursive,
            )
            .context("debouncer request_consent_path")?;

        debouncer
            .watcher()
            .watch(
                history_consent_path!().as_path(),
                RecursiveMode::NonRecursive,
            )
            .context("debouncer history_consent_path")?;

        self.file_observer = Some(debouncer);

        Ok(())
    }

    pub async fn update_general_consent(
        &self,
        desired_consents: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        self.ensure()?;

        let Some(tx) = &self.tx_reported_properties else {
            anyhow::bail!("update_general_consent: tx_reported_properties is None")
        };

        if let Some(desired_consents) = desired_consents {
            let mut new_consents = desired_consents
                .iter()
                .map(|e| match (e.is_string(), e.as_str()) {
                    (true, Some(s)) => Ok(s.to_string().to_lowercase()),
                    _ => Err(anyhow!("cannot parse string from new_consents json.")
                        .context("update_general_consent: parse desired_consents")),
                })
                .collect::<Result<Vec<String>>>()?;

            // enforce entries only exists once
            new_consents.sort_by_key(|name| name.to_string());
            new_consents.dedup();

            let mut current_config: serde_json::Value = serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("update_general_consent: open consent_conf.json for read")?,
            )
            .context("update_general_consent: serde_json::from_reader")?;

            let saved_consents = current_config["general_consent"]
                .as_array()
                .context("update_general_consent: general_consent array malformed")?
                .iter()
                .map(|e| match (e.is_string(), e.as_str()) {
                    (true, Some(s)) => Ok(s.to_string().to_lowercase()),
                    _ => Err(anyhow!("cannot parse string from saved_consents json.")
                        .context("update_general_consent: parse saved_consents")),
                })
                .collect::<Result<Vec<String>>>()?;

            // check if consents changed (current desired vs. saved)
            if new_consents.eq(&saved_consents) {
                info!("desired general_consent didn't change");
                return Ok(());
            }

            current_config["general_consent"] = serde_json::Value::from(new_consents);

            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("update_general_consent: open consent_conf.json for write")?,
                &current_config,
            )
            .context("update_general_consent: serde_json::to_writer_pretty")?;
        } else {
            info!("no general consent defined in desired properties. current general_consent is reported.");
        };

        Self::report_general_consent(tx)
            .await
            .context("update_general_consent: report_general_consent")
    }

    async fn report_general_consent(
        tx_reported_properties: &Sender<serde_json::Value>,
    ) -> Result<()> {
        Self::report_consent(
            tx_reported_properties,
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("report_general_consent: open consent_conf.json fo read")?,
            )
            .context("report_general_consent: serde_json::from_reader")?,
        )
        .await
        .context("report_general_consent: report_consent")
    }

    async fn report_user_consent(
        tx_reported_properties: &Sender<serde_json::Value>,
        report_consent_file: &Path,
    ) -> Result<()> {
        Self::report_consent(
            tx_reported_properties,
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(report_consent_file)
                    .context("report_user_consent: open report_consent_file for read")?,
            )
            .context("report_user_consent: serde_json::from_reader")?,
        )
        .await
        .context("report_user_consent: report_consent")
    }

    async fn report_consent(
        tx_reported_properties: &Sender<serde_json::Value>,
        value: serde_json::Value,
    ) -> Result<()> {
        tx_reported_properties
            .send(json!({ "device_update_consent": value }))
            .await
            .context("report_consent: report_impl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_and_report_general_consent_failed_test() {
        let (tx_reported_properties, _rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let usr_consent = DeviceUpdateConsent {
            file_observer: None,
            tx_reported_properties: Some(tx_reported_properties),
        };

        let err = block_on(async { usr_consent.update_general_consent(None).await }).unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: report_general_consent")));

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("report_general_consent: open consent_conf.json")));

        let err = block_on(async {
            usr_consent
                .update_general_consent(Some(json!([1, 1]).as_array().unwrap()))
                .await
        })
        .unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: parse desired_consents")));

        let err = block_on(async {
            usr_consent
                .update_general_consent(Some(json!(["1", "1"]).as_array().unwrap()))
                .await
        })
        .unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: open consent_conf.json")));
    }
}
