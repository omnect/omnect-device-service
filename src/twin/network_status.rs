use super::super::systemd::networkd;
use super::Feature;
use anyhow::{Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{error, info, warn};
use serde::Serialize;
use serde_json::json;
use std::{any::Any, env};
use tokio::sync::mpsc::Sender;

#[derive(Serialize)]
pub struct Address {
    addr: String,
    prefix_len: u64,
    dhcp: bool,
}

#[derive(Serialize)]
pub struct IpConfig {
    addrs: Vec<Address>,
    gateways: Vec<String>,
    dns: Vec<String>,
}

#[derive(Serialize)]
pub struct Interface {
    name: String,
    mac: String,
    online: bool,
    ipv4: IpConfig,
    //    ipv6: IpConfig,
}

pub struct NetworkStatus {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    interfaces: Vec<Interface>,
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
    const NETWORK_STATUS_VERSION: u8 = 3;
    const ID: &'static str = "network_status";

    pub async fn new() -> Result<Self> {
        Ok(NetworkStatus {
            interfaces: Self::parse_interfaces(&networkd::networkd_interfaces().await?)?,
            tx_reported_properties: None,
        })
    }

    pub async fn refresh(&mut self) -> Result<Option<serde_json::Value>> {
        info!("network status requested");

        self.ensure()?;
        self.interfaces = Self::parse_interfaces(&networkd::networkd_interfaces().await?)?;
        self.report().await?;

        Ok(None)
    }

    async fn report(&self) -> Result<()> {
        let Some(tx) = &self.tx_reported_properties else {
            warn!("report: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({
            "network_status": {
                "interfaces": json!(self.interfaces)
            }
        }))
        .await
        .context("report")
    }

    fn parse_interfaces(json: &serde_json::Value) -> Result<Vec<Interface>> {
        let mut report = vec![];

        if let Some(interfaces) = json["Interfaces"].as_array() {
            let ifs: Vec<&serde_json::Value> = interfaces
                .iter()
                .filter(|i| {
                    i["AdministrativeState"]
                        .as_str()
                        .is_some_and(|s| s != "unmanaged")
                })
                .collect();

            for i in ifs {
                let mut addrs_v4: Vec<Address> = vec![];
                let mut gateways: Vec<String> = vec![];
                let mut dns_server: Vec<String> = vec![];

                let Some(name) = i["Name"].as_str() else {
                    error!("parse Name");
                    continue;
                };
                let Some(online) = i["OnlineState"].as_str() else {
                    error!("parse OnlineState");
                    continue;
                };
                let Ok(mac) =
                    serde_json::from_str::<Vec<u8>>(i["HardwareAddress"].to_string().as_str())
                else {
                    error!("parse HardwareAddress");
                    continue;
                };

                if let Some(addrs) = i["Addresses"].as_array() {
                    addrs_v4 = addrs
                        .iter()
                        .filter_map(|a| {
                            if !(a["Family"].as_u64().is_some_and(|f| f.eq(&2))
                                && a["ScopeString"].as_str().is_some_and(|ss| ss.eq("global")))
                            {
                                return None;
                            }
                            let Ok(addr) =
                                serde_json::from_str::<Vec<u8>>(a["Address"].to_string().as_str())
                            else {
                                error!("parse Address {:?}", a["Address"]);
                                return None;
                            };
                            let Ok(addr) = TryInto::<[u8; 4]>::try_into(addr.as_slice()) else {
                                error!("convert Address {:?}", addr);
                                return None;
                            };
                            let Some(prefix_len) = a["PrefixLength"].as_u64() else {
                                error!("parse PrefixLength {}", a["PrefixLength"]);
                                return None;
                            };
                            let dhcp = a["ConfigSource"].as_str().is_some_and(|cs| cs.eq("DHCPv4"));

                            Some(Address {
                                addr: std::net::IpAddr::from(addr).to_string(),
                                prefix_len,
                                dhcp,
                            })
                        })
                        .collect();
                }

                if let Some(routes) = i["Routes"].as_array() {
                    gateways = routes
                        .iter()
                        .filter_map(|r| {
                            if !(r["Gateway"].is_array()
                                && r["Family"].as_u64().is_some_and(|f| f.eq(&2))
                                && r["ConfigState"]
                                    .as_str()
                                    .is_some_and(|c| c.eq("configured"))
                                && r["ScopeString"].as_str().is_some_and(|c| c.eq("global")))
                            {
                                return None;
                            }
                            let Ok(addr) =
                                serde_json::from_str::<Vec<u8>>(r["Gateway"].to_string().as_str())
                            else {
                                error!("parse gateway {}", r["Gateway"]);
                                return None;
                            };

                            let Ok(addr) = TryInto::<[u8; 4]>::try_into(addr.as_slice()) else {
                                error!("convert gateway {:?}", addr);
                                return None;
                            };

                            Some(std::net::IpAddr::from(addr).to_string())
                        })
                        .collect();
                }

                if let Some(server) = i["DNS"].as_array() {
                    dns_server = server
                        .iter()
                        .filter_map(|d| {
                            if !d["Family"].as_u64().is_some_and(|f| f.eq(&2)) {
                                return None;
                            }
                            let Ok(addr) =
                                serde_json::from_str::<Vec<u8>>(d["Address"].to_string().as_str())
                            else {
                                error!("parse Address {}", d["Address"]);
                                return None;
                            };

                            let Ok(addr) = TryInto::<[u8; 4]>::try_into(addr.as_slice()) else {
                                error!("convert Address {:?}", addr);
                                return None;
                            };

                            Some(std::net::IpAddr::from(addr).to_string())
                        })
                        .collect();
                }

                let ipv4 = IpConfig {
                    addrs: addrs_v4,
                    gateways,
                    dns: dns_server,
                };

                report.push(Interface {
                    name: name.to_string(),
                    online: online.eq("online"),
                    mac: mac
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<String>>()
                        .join(":"),
                    ipv4,
                });
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn networkd_parse_interfaces_ok() {
        let json: serde_json::Value = serde_json::from_reader(
            OpenOptions::new()
                .read(true)
                .create(false)
                .open("testfiles/positive/systemd-networkd-link-description.json")
                .unwrap(),
        )
        .unwrap();

        assert_eq!(NetworkStatus::parse_interfaces(&json).unwrap().len(), 2)
    }
}
