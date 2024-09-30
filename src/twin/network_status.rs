use super::Feature;
use anyhow::{Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::info;

use serde::Serialize;
use serde_json::json;
use serde_with::skip_serializing_none;
use std::{any::Any, env};
use tokio::sync::mpsc::Sender;

#[skip_serializing_none]
#[derive(Serialize)]
struct NetworkReport {
    #[serde(default)]
    name: String,
    mac: String,
    addr_v4: Option<Vec<String>>,
    addr_v6: Option<Vec<String>>,
}

pub struct NetworkStatus {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    interfaces: Vec<NetworkReport>,
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
        self.refresh().await?;
        Ok(())
    }
}

impl NetworkStatus {
    const NETWORK_STATUS_VERSION: u8 = 2;
    const ID: &'static str = "network_status";

    pub fn new() -> Result<Self> {
        let mut ns = NetworkStatus {
            interfaces: vec![],
            tx_reported_properties: None,
        };
        ns.network_status()?;
        Ok(ns)
    }

    pub async fn refresh(&mut self) -> Result<Option<serde_json::Value>> {
        info!("network status requested");

        self.ensure()?;
        self.network_status()?;
        self.report().await?;

        Ok(None)
    }

    #[cfg(not(feature = "mock"))]
    fn network_status(&mut self) -> Result<()> {
        use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
        use std::collections::HashMap;

        let mut interfaces: HashMap<String, NetworkReport> = HashMap::new();

        NetworkInterface::show()
            .context("report_network_status")?
            .iter()
            .for_each(|i| {
                let entry = interfaces.entry(i.name.clone()).or_insert(NetworkReport {
                    addr_v4: None,
                    addr_v6: None,
                    mac: i.mac_addr.clone().unwrap_or_else(|| "none".to_string()),
                    name: i.name.clone(),
                });

                for a in &i.addr {
                    match a {
                        Addr::V4(addr) => entry
                            .addr_v4
                            .get_or_insert(vec![])
                            .push(addr.ip.to_string()),
                        Addr::V6(addr) => entry
                            .addr_v6
                            .get_or_insert(vec![])
                            .push(addr.ip.to_string()),
                    };
                }
            });

        self.interfaces = interfaces.into_values().collect::<Vec<NetworkReport>>();

        Ok(())
    }

    async fn report(&self) -> Result<()> {
        let Some(tx) = &self.tx_reported_properties else {
            anyhow::bail!("report: tx_reported_properties is None")
        };

        tx.send(json!({
            "network_status": {
                "interfaces": json!(self.interfaces)
            }
        }))
        .await
        .context("report")
    }

    #[cfg(feature = "mock")]
    fn network_status(&mut self) -> Result<()> {
        self.interfaces = vec![NetworkReport {
            addr_v4: Some(vec!["0.0.0.0".to_owned()]),
            addr_v6: None,
            name: "eth0".to_owned(),
            mac: "00:11:22:33:44".to_owned(),
        }];

        Ok(())
    }
}
