use super::{Feature, FeatureState};
use crate::consent_path;
use crate::twin;
use crate::twin::Twin;
use crate::Message;
use anyhow::{anyhow, ensure, Context, Result};
use log::{debug, error, info};
use notify::{INotifyWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind, Debouncer};
use serde_json::json;
use std::any::Any;
use std::env;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

#[macro_export]
macro_rules! consent_path {
    () => {{
        static CONSENT_DIR_PATH_DEFAULT: &'static str = "/etc/omnect/consent";
        std::env::var("CONSENT_DIR_PATH").unwrap_or(CONSENT_DIR_PATH_DEFAULT.to_string())
    }};
}

#[macro_export]
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

pub fn user_consent(in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    twin::get_or_init(None).exec(|twin| {
        twin.feature::<DeviceUpdateConsent>()?
            .user_consent(in_json.to_owned())
    })
}

#[derive(Default)]
pub struct DeviceUpdateConsent {
    state: FeatureState,
    file_observer: Option<Debouncer<INotifyWatcher>>,
}

impl Feature for DeviceUpdateConsent {
    fn name(&self) -> String {
        DeviceUpdateConsent::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::USER_CONSENT_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_DEVICE_UPDATE_USER_CONSENT") != Ok("true".to_string())
    }

    fn report_initial_state(&self) -> Result<()> {
        self.ensure()?;
        Self::report_general_consent(self.tx())?;
        Self::report_user_consent(self.tx(), &request_consent_path!())?;
        Self::report_user_consent(self.tx(), &history_consent_path!())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn start(&mut self) -> Result<()> {
        self.ensure()?;
        self.observe_consent()
    }

    fn state_mut(&mut self) -> &mut FeatureState {
        &mut self.state
    }

    fn state(&self) -> &FeatureState {
        &self.state
    }
}

impl DeviceUpdateConsent {
    const USER_CONSENT_VERSION: u8 = 1;
    const ID: &'static str = "device_update_consent";
    const FILE_WATCHER_DELAY_SECS: u64 = 1;

    pub fn user_consent(&self, in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
        info!("user consent requested: {}", in_json);

        self.ensure()?;

        match serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(in_json) {
            //match in_json.pointer(pointer) {
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

    pub fn observe_consent(&mut self) -> Result<()> {
        let tx = Some(
            self.tx()
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("observe_consent: tx channel missing"))?
                .clone(),
        );

        let mut debouncer = new_debouncer(
            Duration::from_secs(Self::FILE_WATCHER_DELAY_SECS),
            None,
            move |res: DebounceEventResult| match res {
                Ok(events) => events.iter().for_each(|e| {
                    debug!("observe_consent: event {:?} for {:?}", e.kind, e.path);

                    if e.kind == DebouncedEventKind::Any {
                        Self::report_user_consent(&tx, &e.path).unwrap_or_else(|e| {
                            error!("observe_consent: twin report user consent: {:#?}", e)
                        });
                    }
                }),
                Err(errors) => errors
                    .iter()
                    .for_each(|e| error!("observe_consent: {:?}", e)),
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

    pub fn update_general_consent(
        &self,
        desired_consents: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        self.ensure()?;

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

            let saved_consents: serde_json::Value = serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("update_general_consent: open consent_conf.json for read")?,
            )
            .context("update_general_consent: serde_json::from_reader")?;

            let saved_consents = saved_consents["general_consent"]
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

            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("update_general_consent: open consent_conf.json for write")?,
                &json!({ "general_consent": new_consents }),
            )
            .context("update_general_consent: serde_json::to_writer_pretty")?;
        } else {
            info!("no general consent defined in desired properties. current general_consent is reported.");
        };

        Self::report_general_consent(self.tx())
            .context("update_general_consent: report_general_consent")
    }

    fn report_general_consent(tx: &Option<Sender<Message>>) -> Result<()> {
        Self::report_consent(
            tx,
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("report_general_consent: open consent_conf.json fo read")?,
            )
            .context("report_general_consent: serde_json::from_reader")?,
        )
        .context("report_general_consent: report_consent")
    }

    fn report_user_consent(tx: &Option<Sender<Message>>, report_consent_file: &Path) -> Result<()> {
        Self::report_consent(
            tx,
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(report_consent_file)
                    .context("report_user_consent: open report_consent_file for read")?,
            )
            .context("report_user_consent: serde_json::from_reader")?,
        )
        .context("report_user_consent: report_consent")
    }

    fn report_consent(tx: &Option<Sender<Message>>, value: serde_json::Value) -> Result<()> {
        Twin::report_impl(tx, json!({ "device_update_consent": value }))
            .context("report_consent: report_impl")
    }
}
