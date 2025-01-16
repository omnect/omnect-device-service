use super::super::systemd::networkd;
use super::web_service;
use super::{feature::*, Feature};
use super::{systemd, systemd::unit::UnitAction};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use serde::Serialize;
use serde_json::json;
use std::{env, time::Duration};
use tokio::{sync::mpsc::Sender, time::interval};

lazy_static! {
    static ref REFRESH_NETWORK_STATUS_INTERVAL_SECS: u64 = {
        const REFRESH_NETWORK_STATUS_INTERVAL_SECS_DEFAULT: &str = "60";
        std::env::var("REFRESH_NETWORK_STATUS_INTERVAL_SECS")
            .unwrap_or(REFRESH_NETWORK_STATUS_INTERVAL_SECS_DEFAULT.to_string())
            .parse::<u64>()
            .expect("cannot parse REFRESH_NETWORK_STATUS_INTERVAL_SECS env var")
    };
}

static NETWORK_SERVICE: &str = "systemd-networkd.service";
static NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS: u64 = 15;

#[derive(PartialEq, Serialize)]
pub struct Address {
    addr: String,
    prefix_len: u64,
    dhcp: bool,
}

#[derive(PartialEq, Serialize)]
pub struct IpConfig {
    addrs: Vec<Address>,
    gateways: Vec<String>,
    dns: Vec<String>,
}

#[derive(PartialEq, Serialize)]
pub struct Interface {
    name: String,
    mac: String,
    online: bool,
    ipv4: IpConfig,
}

#[derive(Default)]
pub struct Network {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    interfaces: Vec<Interface>,
}

#[async_trait(?Send)]
impl Feature for Network {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::NETWORK_STATUS_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_NETWORK_STATUS") != Ok("true".to_string())
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.tx_reported_properties = Some(tx_reported_properties);
        self.report(true).await?;
        Ok(())
    }

    fn event_stream(&mut self) -> EventStreamResult {
        if !self.is_enabled() || 0 == *REFRESH_NETWORK_STATUS_INTERVAL_SECS {
            Ok(None)
        } else {
            Ok(Some(interval_stream::<Network>(interval(
                Duration::from_secs(*REFRESH_NETWORK_STATUS_INTERVAL_SECS),
            ))))
        }
    }

    async fn command(&mut self, cmd: Command) -> CommandResult {
        match cmd {
            Command::Interval(_) => {}
            Command::ReloadNetwork => {
                systemd::unit::unit_action(
                    NETWORK_SERVICE,
                    UnitAction::Reload,
                    Duration::from_secs(NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS),
                )
                .await?
            }
            _ => bail!("unexpected command"),
        }

        self.report(false).await?;

        Ok(None)
    }
}

impl Network {
    const NETWORK_STATUS_VERSION: u8 = 3;
    const ID: &'static str = "network_status";

    async fn report(&mut self, force: bool) -> Result<()> {
        let interfaces = Self::parse_interfaces(&networkd::networkd_interfaces().await?)?;

        // only report on change
        let interfaces = match self.interfaces.eq(&interfaces) {
            true if !force => {
                debug!("network status didn't change");
                return Ok(());
            }
            _ => {
                info!("network status changed");
                self.interfaces = interfaces;
                json!(self.interfaces)
            }
        };

        web_service::publish(
            web_service::PublishChannel::NetworkStatus,
            json!({"network_status": interfaces}),
        )
        .await;

        let Some(tx) = &self.tx_reported_properties else {
            warn!("report: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({
            "network_status": {
                "interfaces": interfaces
            }
        }))
        .await
        .context("report twin")
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
                    error!("parse_interfaces: skip interface ('Name' missing)");
                    continue;
                };
                let Some(online) = i["OnlineState"].as_str() else {
                    error!("parse_interfaces: skip interface ('OnlineState' missing)");
                    continue;
                };
                let Some(mac: <Vec<Value<u8>>>) = i["HardwareAddress"].as_array() else{continue;};
                let Ok(mac) =
                    serde_json::from_str::<Vec<u8>>(i["HardwareAddress"].to_string().as_str())
                else {
                    error!("parse_interfaces: skip interface ('HardwareAddress' missing format: {:?})");
                    continue;
                };
                if 6 != mac.len() {
                    error!(
                        "parse_interfaces: skip interface ('HardwareAddress' length: {:?})",
                        mac
                    );
                    continue;
                }
                let mac = mac
                    .iter()
                    .map(|v| format!("{:02x}", v).to_string())
                    .collect::<Vec<String>>()
                    .join(":");

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
                                error!(
                                    "parse_interfaces: skip interface ('Address' format: {:?})",
                                    a["Address"]
                                );
                                return None;
                            };
                            let Ok(addr) = TryInto::<[u8; 4]>::try_into(addr.as_slice()) else {
                                error!(
                                    "parse_interfaces: skip interface ('Address' length: {:?})",
                                    addr
                                );
                                return None;
                            };
                            let Some(prefix_len) = a["PrefixLength"].as_u64() else {
                                error!("parse_interfaces: skip interface ('PrefixLength' missing)");
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
                                error!("parse_interfaces: skip interface ('Gateway' unexpected format)");
                                return None;
                            };

                            let Ok(addr) = TryInto::<[u8; 4]>::try_into(addr.as_slice()) else {
                                error!("parse_interfaces: skip interface ('Gateway' unexpected array)");
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
                                error!(
                                    "parse_interfaces: skip interface ('DNS' unexpected format)"
                                );
                                return None;
                            };

                            let Ok(addr) = TryInto::<[u8; 4]>::try_into(addr.as_slice()) else {
                                error!("parse_interfaces: skip interface ('DNS' unexpected array)");
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
                    mac,
                    ipv4,
                });
            }
        }

        // we always sort vector by name to make it comparable
        report.sort_by_key(|k| k.name.clone());

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

        assert_eq!(Network::parse_interfaces(&json).unwrap().len(), 2)
    }
}
