use crate::{
    systemd::{networkd, unit},
    twin::{Feature, feature::*},
    web_service,
};
use anyhow::{Context, Result, bail};
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
        env::var("REFRESH_NETWORK_STATUS_INTERVAL_SECS")
            .unwrap_or(REFRESH_NETWORK_STATUS_INTERVAL_SECS_DEFAULT.to_string())
            .parse::<u64>()
            .expect("cannot parse REFRESH_NETWORK_STATUS_INTERVAL_SECS env var")
    };
}

static NETWORK_SERVICE: &str = "systemd-networkd.service";

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
    file: String,
    ipv4: IpConfig,
}

#[derive(Default)]
pub struct Network {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    interfaces: Vec<Interface>,
}

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

    fn command_request_stream(&mut self) -> CommandRequestStreamResult {
        if !self.is_enabled() || 0 == *REFRESH_NETWORK_STATUS_INTERVAL_SECS {
            Ok(None)
        } else {
            Ok(Some(interval_stream::<Network>(interval(
                Duration::from_secs(*REFRESH_NETWORK_STATUS_INTERVAL_SECS),
            ))))
        }
    }

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        match cmd {
            Command::Interval(_) => {}
            Command::ReloadNetwork => {
                unit::unit_action(
                    NETWORK_SERVICE,
                    unit::UnitAction::Reload,
                    systemd_zbus::Mode::Fail,
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
            web_service::PublishChannel::NetworkStatusV1,
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
                let Some(file) = i["NetworkFile"].as_str() else {
                    error!("parse_interfaces: skip interface ('NetworkFile' missing)");
                    continue;
                };

                let Some(mac) = Self::parse_mac(i, "HardwareAddress") else {
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

                            let addr = Self::parse_ipv4(a, "Address")?;

                            let Some(prefix_len) = a["PrefixLength"].as_u64() else {
                                error!("parse_interfaces: skip interface ('PrefixLength' missing)");
                                return None;
                            };
                            let dhcp = a["ConfigSource"].as_str().is_some_and(|cs| cs.eq("DHCPv4"));

                            Some(Address {
                                addr,
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

                            Self::parse_ipv4(r, "Gateway")
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

                            Self::parse_ipv4(d, "Address")
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
                    file: file.to_string(),
                    ipv4,
                });
            }
        }

        // we always sort vector by name to make it comparable
        report.sort_by_key(|k| k.name.clone());

        Ok(report)
    }

    fn parse_mac(value: &serde_json::Value, field: &str) -> Option<String> {
        let mac = value[field].to_string();
        let Ok(mac) = serde_json::from_str::<[u8; 6]>(&mac) else {
            error!(
                "parse_interfaces: skip interface ('{field}' missing or invalid format: {:?})",
                mac
            );
            return None;
        };
        Some(
            mac.iter()
                .map(|v| format!("{:02x}", v).to_string())
                .collect::<Vec<String>>()
                .join(":"),
        )
    }

    fn parse_ipv4(value: &serde_json::Value, field: &str) -> Option<String> {
        let addr = value[field].to_string();
        let Ok(addr) = serde_json::from_str::<[u8; 4]>(&addr) else {
            error!(
                "parse_interfaces: skip interface ('{field}' missing or invalid format: {:?})",
                addr
            );
            return None;
        };

        Some(std::net::IpAddr::from(addr).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::from_json_file;

    #[test]
    fn networkd_parse_interfaces_ok() {
        let json: serde_json::Value =
            from_json_file("testfiles/positive/systemd-networkd-link-description.json").unwrap();

        assert_eq!(Network::parse_interfaces(&json).unwrap().len(), 2)
    }

    #[test]
    fn networkd_parse_interfaces_missing_mac() {
        let json: serde_json::Value =
            from_json_file("testfiles/negative/systemd-networkd-link-description-missing-mac.json")
                .unwrap();

        assert_eq!(Network::parse_interfaces(&json).unwrap().len(), 1)
    }
}
