use super::{feature, Feature};
use anyhow::{Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use lazy_static::lazy_static;
use serde_json::json;
use std::{any::Any, env, time::Duration};
use tokio::{sync::mpsc::Sender, time::interval};

#[cfg(feature = "modem_info")]
mod inner {
    use super::*;

    use futures::future::join_all;
    use log::{debug, info, warn};
    use modemmanager::dbus::{bearer, modem, modem3gpp, sim};
    use modemmanager::types::ModemCapability;
    use serde::Serialize;
    use std::collections::HashMap;
    use tokio::sync::OnceCell;
    use zbus::{
        fdo::{DBusProxy, ObjectManagerProxy},
        zvariant::OwnedObjectPath,
        Connection,
    };

    #[derive(PartialEq, Serialize)]
    struct SimProperties {
        operator: String,
        iccid: String,
    }

    #[derive(PartialEq, Serialize)]
    struct ModemReport {
        manufacturer: String,
        model: String,
        revision: String,
        preferred_technologies: Vec<ModemCapability>,
        #[serde(skip_serializing_if = "Option::is_none")]
        imei: Option<String>,
        sims: Vec<SimProperties>,
        bearers: Vec<serde_json::Value>,
    }

    pub struct ModemInfo {
        connection: OnceCell<Connection>,
        modem_reports: Vec<ModemReport>,
        pub(super) tx_reported_properties: Option<Sender<serde_json::Value>>,
    }

    impl ModemInfo {
        const MODEMMANAGER_BUS: &'static str = "org.freedesktop.ModemManager1";
        const MODEMMANAGER_3GPP_BUS: &'static str = "org.freedesktop.ModemManager1.Modem3gpp";
        const MODEMMANAGER_PATH: &'static str = "/org/freedesktop/ModemManager1";

        async fn connection(&self) -> Result<&Connection> {
            self.connection
                .get_or_try_init(|| async {
                    Connection::system()
                        .await
                        .context("failed to setup connection".to_string())
                })
                .await
        }

        async fn modem_paths(&self) -> Result<Vec<OwnedObjectPath>> {
            let connection = self.connection().await?;
            let dbus = DBusProxy::new(connection).await?;

            if !dbus
                .list_names()
                .await?
                .into_iter()
                .any(|elem| elem.as_str() == Self::MODEMMANAGER_BUS)
            {
                return Ok(vec![]);
            }

            let managed_objects = ObjectManagerProxy::builder(connection)
                .destination(Self::MODEMMANAGER_BUS)?
                .path(Self::MODEMMANAGER_PATH)?
                .build()
                .await?
                .get_managed_objects()
                .await?;

            Ok(managed_objects.keys().cloned().collect::<Vec<_>>())
        }

        async fn bearer_proxy<'a>(
            &self,
            bearer_path: &'a OwnedObjectPath,
        ) -> Result<bearer::BearerProxy<'a>> {
            let connection = self.connection().await?;

            bearer::BearerProxy::builder(connection)
                .destination(Self::MODEMMANAGER_BUS)?
                .path(bearer_path)?
                .build()
                .await
                .map_err(|err| anyhow::anyhow!("failed to construct bearer proxy: {err}"))
        }

        async fn sim_proxy<'a>(&self, sim_path: &'a OwnedObjectPath) -> Result<sim::SimProxy<'a>> {
            let connection = self.connection().await?;

            sim::SimProxy::builder(connection)
                .destination(Self::MODEMMANAGER_BUS)?
                .path(sim_path)?
                .build()
                .await
                .map_err(|err| anyhow::anyhow!("failed to construct sim proxy: {err}"))
        }

        async fn modem_proxy<'a>(
            &self,
            modem_path: &'a OwnedObjectPath,
        ) -> Result<modem::ModemProxy<'a>> {
            let connection = self.connection().await?;

            modem::ModemProxy::builder(connection)
                .destination(Self::MODEMMANAGER_BUS)?
                .path(modem_path)?
                .build()
                .await
                .map_err(|err| anyhow::anyhow!("failed to construct modem proxy: {err}"))
        }

        async fn modem_3gpp_proxy<'a>(
            &self,
            modem_path: &'a OwnedObjectPath,
        ) -> Result<modem3gpp::Modem3gppProxy<'a>> {
            let connection = self.connection().await?;

            modem3gpp::Modem3gppProxy::builder(connection)
                .destination(Self::MODEMMANAGER_BUS)?
                .path(modem_path)?
                .build()
                .await
                .map_err(|err| anyhow::anyhow!("failed to construct modem 3gpp proxy: {err}"))
        }

        pub fn new() -> Self {
            ModemInfo {
                connection: OnceCell::new(),
                modem_reports: vec![],
                tx_reported_properties: None,
            }
        }

        pub async fn report(&mut self, force: bool) -> Result<()> {
            let modem_reports = join_all(
                self.modem_paths()
                    .await?
                    .iter()
                    .map(|modem_path| self.modem_status(modem_path)),
            )
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .context("failed querying modem status")?;

            // only report on change
            let modem_reports = match self.modem_reports.eq(&modem_reports) {
                true if !force => {
                    debug!("modem status didn't change");
                    return Ok(());
                }
                _ => {
                    info!("modem status changed");
                    self.modem_reports = modem_reports;
                    json!({
                        "modem_info": {
                            "modems": self.modem_reports,
                        }
                    })
                }
            };

            let Some(tx) = &self.tx_reported_properties else {
                warn!("skip since tx_reported_properties is None");
                return Ok(());
            };

            tx.send(modem_reports).await.context("report")
        }

        async fn bearer_properties<'a>(
            &self,
            modem: &modem::ModemProxy<'a>,
        ) -> Result<Vec<serde_json::Value>> {
            let bearers_paths = modem.bearers().await?.to_vec();

            join_all(bearers_paths.iter().map(|bearer_path| async {
                let bearer = self.bearer_proxy(bearer_path).await?;

                let bearer_properties = bearer.properties().await?;

                Ok(serde_json::to_value(
                    bearer_properties
                        .into_iter()
                        .map(|(key, value)| (key, value.to_string()))
                        .collect::<HashMap<_, _>>(),
                )?)
            }))
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
        }

        async fn sim_property(&self, sim_path: &OwnedObjectPath) -> Result<SimProperties> {
            let sim = self.sim_proxy(sim_path).await?;

            let operator = sim.operator_name().await?;
            let iccid = sim.sim_identifier().await?;

            Ok(SimProperties { operator, iccid })
        }

        async fn sims_properties<'a>(
            &self,
            modem: &modem::ModemProxy<'a>,
        ) -> Result<Vec<SimProperties>> {
            let mut sim_paths = modem.sim_slots().await?;

            if sim_paths.is_empty() {
                // Workaround: there are some devices where the sim slots are
                // reported as an empty list albeit the system having a single sim,
                // try to report this sim.

                let active_sim_path = modem.sim().await?;
                if !active_sim_path.is_empty() && active_sim_path.as_str() != "/" {
                    sim_paths.push(active_sim_path);
                }
            }

            join_all(sim_paths.iter().filter_map(|sim_path| {
                // Workaround: ModemManager appears to report empty SIM slots as
                // "/"
                if !sim_path.is_empty() && sim_path.as_str() != "/" {
                    Some(self.sim_property(sim_path))
                } else {
                    None
                }
            }))
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
        }

        async fn modem_status(&self, modem_path: &OwnedObjectPath) -> Result<ModemReport> {
            let modem = self.modem_proxy(modem_path).await?;

            let manufacturer = modem.manufacturer().await?;
            let model = modem.model().await?;
            let revision = modem.revision().await?;
            let preferred_technologies = modem.supported_capabilities().await?;
            let bearers = self.bearer_properties(&modem).await?;
            let sims = self.sims_properties(&modem).await?;

            let connection = self.connection().await?;
            let dbus = DBusProxy::new(connection).await?;
            let imei = if dbus
                .list_names()
                .await?
                .into_iter()
                .any(|elem| elem.as_str() == Self::MODEMMANAGER_3GPP_BUS)
            {
                let modem_3gpp = self.modem_3gpp_proxy(modem_path).await;

                if let Ok(modem_3gpp) = modem_3gpp {
                    Some(modem_3gpp.imei().await?)
                } else {
                    None
                }
            } else {
                None
            };

            Ok(ModemReport {
                manufacturer,
                model,
                revision,
                preferred_technologies,
                imei,
                sims,
                bearers,
            })
        }
    }
}

#[cfg(not(feature = "modem_info"))]
mod inner {
    use super::*;
    use log::warn;

    #[derive(Default)]
    pub struct ModemInfo {
        pub(super) tx_reported_properties: Option<Sender<serde_json::Value>>,
    }

    impl ModemInfo {
        pub fn new() -> Self {
            ModemInfo::default()
        }

        pub async fn report(&self, force: bool) -> Result<()> {
            self.ensure()?;

            let Some(tx) = &self.tx_reported_properties else {
                warn!("skip since tx_reported_properties is None");
                return Ok(());
            };

            if force {
                tx.send(json!({
                    "modem_info": {
                        "modems": [],
                    }
                }))
                .await
                .context("report")?;
            }

            Ok(())
        }
    }
}

pub use inner::ModemInfo;

const MODEM_INFO_VERSION: u8 = 1;
const ID: &str = "modem_info";

lazy_static! {
    static ref REFRESH_MODEM_INFO_INTERVAL_SECS: u64 = {
        const REFRESH_MODEM_INFO_INTERVAL_SECS_DEFAULT: &str = "600";
        std::env::var("REFRESH_MODEM_INFO_INTERVAL_SECS")
            .unwrap_or(REFRESH_MODEM_INFO_INTERVAL_SECS_DEFAULT.to_string())
            .parse::<u64>()
            .expect("cannot parse REFRESH_MODEM_INFO_INTERVAL_SECS env var")
    };
}

#[async_trait(?Send)]
impl Feature for ModemInfo {
    fn name(&self) -> String {
        ID.to_string()
    }

    fn version(&self) -> u8 {
        MODEM_INFO_VERSION
    }

    fn is_enabled(&self) -> bool {
        match env::var("DISTRO_FEATURES") {
            Ok(features) => features.split_whitespace().any(|feature| feature == "3g"),
            _ => false,
        }
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
        self.tx_reported_properties = Some(tx_reported_properties);
        self.report(true).await
    }

    fn refresh_event(&self) -> Option<feature::StreamResult> {
        if !self.is_enabled() || 0 == *REFRESH_MODEM_INFO_INTERVAL_SECS {
            None
        } else {
            Some(feature::interval_stream::<ModemInfo>(interval(
                Duration::from_secs(*REFRESH_MODEM_INFO_INTERVAL_SECS),
            )))
        }
    }

    async fn refresh(&mut self, payload: &feature::EventData) -> Result<()> {
        self.ensure()?;
        self.report(false).await
    }
}
