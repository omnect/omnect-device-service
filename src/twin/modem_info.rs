use super::Feature;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::join_all;
use log::{debug, info};
use serde::Serialize;
use serde_json::json;
use std::{any::Any, env};
use tokio::sync::mpsc::Sender;
use zbus::{
    fdo::{DBusProxy, ObjectManagerProxy},
    zvariant::OwnedObjectPath,
    Connection,
};

use modemmanager::dbus::{bearer, modem, modem3gpp, sim};
use modemmanager_sys::MMModemCapability;

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
    preferred_technologies: Vec<MMModemCapability>,
    imei: Option<String>,
    sims: Vec<SimProperties>,
    bearers: Vec<BearerProperties>,
}

pub struct ModemInfo {
    connection: Connection,
    tx_reported_properties: Sender<serde_json::Value>,
}

#[async_trait(?Send)]
impl Feature for ModemInfo {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::MODEM_INFO_VERSION
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

impl ModemInfo {
    const MODEMMANAGER_BUS: &'static str = "org.freedesktop.ModemManager1";
    const MODEMMANAGER_3GPP_BUS: &'static str = "org.freedesktop.ModemManager1.Modem3gpp";
    const MODEMMANAGER_PATH: &'static str = "/org/freedesktop/ModemManager1";

    const MODEM_INFO_VERSION: u8 = 1;
    const ID: &'static str = "modem_info";

    async fn modem_paths(&self) -> Result<Vec<OwnedObjectPath>> {
        let dbus = DBusProxy::new(&self.connection).await?;

        if !dbus
            .list_names()
            .await?
            .into_iter()
            .any(|elem| elem.as_str() == Self::MODEMMANAGER_BUS)
        {
            return Ok(vec![]);
        }

        let managed_objects = ObjectManagerProxy::builder(&self.connection)
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
        bearer::BearerProxy::builder(&self.connection)
            .destination(Self::MODEMMANAGER_BUS)?
            .path(bearer_path)?
            .build()
            .await
            .map_err(|err| anyhow::anyhow!("failed to construct bearer proxy: {err}"))
    }

    async fn sim_proxy<'a>(&self, sim_path: &'a OwnedObjectPath) -> Result<sim::SimProxy<'a>> {
        sim::SimProxy::builder(&self.connection)
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
        modem::ModemProxy::builder(&self.connection)
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
        modem3gpp::Modem3gppProxy::builder(&self.connection)
            .destination(Self::MODEMMANAGER_BUS)?
            .path(modem_path)?
            .build()
            .await
            .map_err(|err| anyhow::anyhow!("failed to construct modem 3gpp proxy: {err}"))
    }

    pub async fn new(tx_reported_properties: Sender<serde_json::Value>) -> Result<Self> {
        let connection = Connection::system()
            .await
            .with_context(|| format!("failed to setup connection"))?;

        Ok(ModemInfo {
            connection,
            tx_reported_properties,
        })
    }

    pub async fn refresh_modem_info(&self) -> Result<Option<serde_json::Value>> {
        info!("modem info status requested");

        self.ensure()?;

        self.report_modem_info_status().await?;

        Ok(None)
    }

    async fn bearer_properties(&self, bearer_path: &OwnedObjectPath) -> Result<BearerProperties> {
        let bearer = self.bearer_proxy(bearer_path).await?;

        let properties = match bearer.properties().await? {
            bearer::Properties::Prop3Gpp(props) => BearerProperties::Bearer3gpp {
                apn: props.apn,
                roaming: props.allow_roaming,
            },
            _ => BearerProperties::UnsupportedBearer,
        };

        Ok(properties)
    }

    async fn sim_properties(&self, sim_path: &OwnedObjectPath) -> Result<SimProperties> {
        let sim = self.sim_proxy(sim_path).await?;

        let operator = sim.operator_name().await?;
        let iccid = sim.sim_identifier().await?;

        Ok(SimProperties { operator, iccid })
    }

    async fn modem_status(&self, modem_path: &OwnedObjectPath) -> Result<ModemReport> {
        let modem = self.modem_proxy(modem_path).await?;

        let manufacturer = modem.manufacturer().await?;
        let model = modem.model().await?;
        let revision = modem.revision().await?;
        let preferred_technologies = modem.supported_capabilities().await?.to_vec();

        let bearers_paths = modem.bearers().await?;
        let bearers = join_all(
            bearers_paths
                .iter()
                .map(|bearer_path| self.bearer_properties(bearer_path)),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

        let sims_paths = modem.sim_slots().await?;
        let sims = join_all(sims_paths.iter().filter_map(|sim_path| {
            // ModemManager appears to report empty SIM slots as "/"
            if !sim_path.is_empty() && sim_path.as_str() != "/" {
                Some(self.sim_properties(sim_path))
            } else {
                None
            }
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

        let dbus = DBusProxy::new(&self.connection).await?;
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

    async fn report_modem_info_status(&self) -> Result<()> {
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

        #[derive(Serialize)]
        struct StatusReport {
            reports: Vec<ModemReport>,
        }

        self.tx_reported_properties
            .send(json!({
                "modem_info": {
                    "modems": modem_reports,
                }
            }))
            .await
            .context("report_modem_info_status: report_impl")
    }
}
