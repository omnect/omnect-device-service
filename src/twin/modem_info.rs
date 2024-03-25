use super::Feature;
use anyhow::Result;
use async_trait::async_trait;
use log::info;
use std::{any::Any, env};
use tokio::sync::mpsc::Sender;

#[cfg(feature = "modem_info")]
mod inner {
    use super::*;

    use anyhow::{Context, Result};
    use futures::future::join_all;
    use log::debug;
    use modemmanager::dbus::{bearer, modem, modem3gpp, sim};
    use modemmanager::types::ModemCapability;
    use serde::Serialize;
    use serde_json::json;
    use tokio::sync::OnceCell;
    use zbus::{
        fdo::{DBusProxy, ObjectManagerProxy},
        zvariant::OwnedObjectPath,
        Connection,
    };

    #[derive(Serialize)]
    enum BearerProperties {
        Bearer3gpp { apn: String, roaming: bool },
        UnsupportedBearer,
    }

    #[derive(Serialize)]
    struct SimProperties {
        operator: String,
        iccid: String,
    }

    #[derive(Serialize)]
    struct ModemReport {
        manufacturer: String,
        model: String,
        revision: String,
        preferred_technologies: Vec<ModemCapability>,
        #[serde(skip_serializing_if = "Option::is_none")]
        imei: Option<String>,
        sims: Vec<SimProperties>,
        bearers: Vec<BearerProperties>,
    }

    pub struct ModemInfo {
        connection: OnceCell<Connection>,
        tx_reported_properties: Sender<serde_json::Value>,
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
                        .with_context(|| "failed to setup connection".to_string())
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

        pub fn new(tx_reported_properties: Sender<serde_json::Value>) -> Self {
            ModemInfo {
                connection: OnceCell::new(),
                tx_reported_properties,
            }
        }

        pub async fn refresh_modem_info(&self) -> Result<Option<serde_json::Value>> {
            info!("modem info status requested");

            self.ensure()?;

            self.report_modem_info().await?;

            Ok(None)
        }

        async fn bearer_properties<'a>(
            &self,
            modem: &modem::ModemProxy<'a>,
        ) -> Result<Vec<BearerProperties>> {
            let bearers_paths = modem.bearers().await?.to_vec();

            join_all(bearers_paths.iter().map(|bearer_path| async {
                let bearer = self.bearer_proxy(bearer_path).await?;

                let properties = match bearer.properties().await? {
                    bearer::Properties::Prop3Gpp(props) => BearerProperties::Bearer3gpp {
                        apn: props.apn,
                        roaming: props.allow_roaming,
                    },
                    _ => BearerProperties::UnsupportedBearer,
                };

                Ok(properties)
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

        async fn sim_properties<'a>(
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
            let sims = self.sim_properties(&modem).await?;

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

        async fn report_modem_info(&self) -> Result<()> {
            self.ensure()?;

            debug!("report_modem_status");

            let modem_paths = self.modem_paths().await?;

            let modem_reports = join_all(
                modem_paths
                    .iter()
                    .map(|modem_path| self.modem_status(modem_path)),
            )
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .with_context(|| "failed querying modem status")?;

            self.tx_reported_properties
                .send(json!({
                    "modem_info": {
                        "modems": modem_reports,
                    }
                }))
                .await
                .context("report_modem_info: report_impl")
        }
    }
}

#[cfg(not(feature = "modem_info"))]
mod inner {
    use super::*;

    pub struct ModemInfo {}

    impl ModemInfo {
        pub fn new(_tx_reported_properties: Sender<serde_json::Value>) -> Self {
            ModemInfo {}
        }

        pub async fn refresh_modem_info(&self) -> Result<Option<serde_json::Value>> {
            info!("modem info status requested");

            self.ensure()?;

            Ok(None)
        }
    }
}

pub use inner::ModemInfo;

const MODEM_INFO_VERSION: u8 = 1;
const ID: &str = "modem_info";

#[async_trait(?Send)]
impl Feature for ModemInfo {
    fn name(&self) -> String {
        ID.to_string()
    }

    fn version(&self) -> u8 {
        MODEM_INFO_VERSION
    }

    async fn report_initial_state(&self) -> Result<()> {
        self.ensure()?;

        self.refresh_modem_info().await?;

        Ok(())
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
}
