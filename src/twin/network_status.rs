use super::Feature;
use anyhow::{Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{debug, error, info};
use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use serde::Serialize;
use serde_json::json;
use serde_with::skip_serializing_none;
use std::{any::Any, collections::HashMap, env};
use tokio::sync::mpsc::Sender;

#[derive(Default)]
pub struct NetworkStatus {
    include_network_filter: Option<Vec<String>>,
    tx_reported_properties: Option<Sender<serde_json::Value>>,
}

#[async_trait(?Send)]
impl Feature for NetworkStatus {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::NETWORK_STATUS_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_NETWORK_STATUS") != Ok("true".to_string())
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

        self.tx_reported_properties = Some(tx_reported_properties);

        Ok(())
    }
}

impl NetworkStatus {
    const NETWORK_STATUS_VERSION: u8 = 1;
    const ID: &'static str = "network_status";

    pub async fn refresh_network_status(&self) -> Result<Option<serde_json::Value>> {
        info!("network status requested");

        self.ensure()?;

        self.report_network_status().await?;

        Ok(None)
    }

    pub async fn update_include_network_filter(
        &mut self,
        include_network_filter: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        self.ensure()?;

        debug!(
            "update_include_network_filter: current filter={:?}",
            self.include_network_filter
        );

        debug!("update_include_network_filter: new filter={include_network_filter:?}");

        if include_network_filter.is_none() {
            if self.include_network_filter.take().is_some() {
                return self.report_network_status().await;
            }

            return Ok(());
        }

        let mut new_include_network_filter: Vec<String> = include_network_filter
            .unwrap()
            .iter()
            .filter(|e| {
                if !e.is_string() {
                    error!("unexpected format in desired include_network_filter. ignore: {e:#}");
                }
                e.is_string()
            })
            .map(|e| e.as_str().unwrap().to_string().to_lowercase())
            .collect();

        // enforce entries only exist once
        new_include_network_filter.sort();
        new_include_network_filter.dedup();

        // check if desired include_network_filter changed
        if self.include_network_filter.is_none()
            || self
                .include_network_filter
                .as_ref()
                .unwrap()
                .ne(&new_include_network_filter)
        {
            self.include_network_filter
                .replace(new_include_network_filter);

            debug!(
                "update_include_network_filter: resulting filter={:?}",
                self.include_network_filter
            );

            self.report_network_status().await
        } else {
            info!("desired include_network_filter didn't change");
            Ok(())
        }
    }

    async fn report_network_status(&self) -> Result<()> {
        self.ensure()?;

        debug!(
            "report_network_status: filter={:?}",
            self.include_network_filter
        );

        let Some(tx) = &self.tx_reported_properties else {
            anyhow::bail!("update_general_consent: tx_reported_properties is None")
        };

        if self.include_network_filter.is_none() {
            return tx
                .send(json!({
                       "network_status": {
                           "interfaces": json!(null)
                   }
                }))
                .await
                .context("report_network_status: report_impl");
        }

        #[skip_serializing_none]
        #[derive(Serialize)]
        struct NetworkReport {
            #[serde(default)]
            name: String,
            mac: String,
            addr_v4: Option<Vec<String>>,
            addr_v6: Option<Vec<String>>,
        }

        let mut interfaces: HashMap<String, NetworkReport> = HashMap::new();

        NetworkInterface::show()
            .context("report_network_status")?
            .iter()
            .filter(|i| {
                self.include_network_filter
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|f| {
                        let name = i.name.to_lowercase();
                        match (f.starts_with('*'), f.ends_with('*'), f.len()) {
                            (_, _, 0) => false,                                     // ""
                            (a, b, 1) if a || b => true,                            // "*"
                            (true, true, len) => name.contains(&f[1..len - 1]),     // ""*...*"
                            (true, false, len) => name.ends_with(&f[1..len]),       // "*..."
                            (false, true, len) => name.starts_with(&f[0..len - 1]), // "...*"
                            _ => name.eq(f),                                        // "..."
                        }
                    })
            })
            .for_each(|i| {
                let entry = interfaces.entry(i.name.clone()).or_insert(NetworkReport {
                    addr_v4: None,
                    addr_v6: None,
                    mac: i.mac_addr.clone().unwrap_or_else(|| "none".to_string()),
                    name: i.name.clone(),
                });

                match i.addr {
                    Some(Addr::V4(addr)) => entry
                        .addr_v4
                        .get_or_insert(vec![])
                        .push(addr.ip.to_string()),
                    Some(Addr::V6(addr)) => entry
                        .addr_v6
                        .get_or_insert(vec![])
                        .push(addr.ip.to_string()),
                    None => error!("report_network_status: ip address is missing"),
                };
            });

        tx.send(json!({
            "network_status": {
                "interfaces": json!(interfaces.into_values().collect::<Vec<NetworkReport>>())
            }
        }))
        .await
        .context("report_network_status")
    }
}
