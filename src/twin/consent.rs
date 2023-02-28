use super::Twin;
use crate::consent_path;
use anyhow::{anyhow, Context, Result};
use log::info;
use serde_json::json;
use std::fs::OpenOptions;

pub fn user_consent(in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("user consent requested: {}", in_json);

    match serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(in_json) {
        Ok(map) if map.len() == 1 && map.values().next().unwrap().is_string() => {
            let (component, version) = map.iter().next().unwrap();
            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(format!(
                        "{}/{}/user_consent.json",
                        consent_path!(),
                        component
                    ))
                    .context("user_consent: open user_consent.json for write")?,
                &json!({ "consent": version }),
            )
            .context("user_consent: serde_json::to_writer_pretty")?;

            Ok(None)
        }
        _ => anyhow::bail!("unexpected parameter format"),
    }
}

impl Twin {
    pub fn update_general_consent(
        &mut self,
        desired_consents: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        if desired_consents.is_some() {
            let mut new_consents = desired_consents
                .unwrap()
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

        self.report_general_consent()
            .context("update_general_consent: report_general_consent")
    }

    pub fn report_general_consent(&mut self) -> Result<()> {
        self.report_impl(
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("report_general_consent: open consent_conf.json fo read")?,
            )
            .context("report_general_consent: serde_json::from_reader")?,
        )
        .context("report_general_consent: report_impl")
    }

    pub fn report_user_consent(&mut self, report_consent_file: &str) -> Result<()> {
        self.report_impl(
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(report_consent_file)
                    .context("report_user_consent: open report_consent_file fo read")?,
            )
            .context("report_user_consent: serde_json::from_reader")?,
        )
        .context("report_user_consent: report_impl")
    }
}
