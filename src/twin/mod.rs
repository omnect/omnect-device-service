mod consent;
mod factory_reset;
#[cfg(feature = "mock")]
#[path = "mod_test.rs"]
mod mod_test;
mod network_status;
mod reboot;
mod ssh;
mod wifi_commissioning;
use super::systemd;
use super::update_validation;
use crate::{
    systemd::WatchdogHandler,
    twin::{
        consent::DeviceUpdateConsent, factory_reset::FactoryReset, network_status::NetworkStatus,
        reboot::Reboot, ssh::Ssh, wifi_commissioning::WifiCommissioning,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::*;
use dotenvy;
use enum_dispatch::enum_dispatch;
use futures_util::FutureExt;
use log::{error, info};
use serde_json::json;
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    path::Path,
};
#[cfg(test)]
use strum::EnumCount;
use strum_macros::EnumCount as EnumCountMacro;
use tokio::{
    select,
    sync::mpsc,
    time::{interval, timeout, Duration},
};

#[enum_dispatch]
#[derive(EnumCountMacro)]
enum TwinFeature {
    FactoryReset,
    DeviceUpdateConsent,
    NetworkStatus,
    Ssh,
}

#[async_trait(?Send)]
#[enum_dispatch(TwinFeature)]
trait Feature {
    fn name(&self) -> String;

    fn version(&self) -> u8;

    fn is_enabled(&self) -> bool;

    fn as_any(&self) -> &dyn Any;

    async fn report_initial_state(&self) -> Result<()> {
        Ok(())
    }

    fn ensure(&self) -> Result<()> {
        if !self.is_enabled() {
            bail!("feature disabled: {}", self.name());
        }

        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        unimplemented!();
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct Twin {
    iothub_client: Box<dyn IotHub>,
    authenticated_once: bool,
    tx_reported_properties: mpsc::Sender<serde_json::Value>,
    features: HashMap<TypeId, Box<dyn Feature>>,
}

impl Twin {
    pub fn new(
        client: Box<dyn IotHub>,
        tx_reported_properties: mpsc::Sender<serde_json::Value>,
    ) -> Self {
        Twin {
            iothub_client: client,
            tx_reported_properties: tx_reported_properties.clone(),
            authenticated_once: false,
            features: HashMap::from([
                (
                    TypeId::of::<DeviceUpdateConsent>(),
                    Box::new(DeviceUpdateConsent::new(tx_reported_properties.clone()))
                        as Box<dyn Feature>,
                ),
                (
                    TypeId::of::<FactoryReset>(),
                    Box::new(FactoryReset::new(tx_reported_properties.clone())) as Box<dyn Feature>,
                ),
                (
                    TypeId::of::<NetworkStatus>(),
                    Box::new(NetworkStatus::new(tx_reported_properties.clone()))
                        as Box<dyn Feature>,
                ),
                (
                    TypeId::of::<Reboot>(),
                    Box::<Reboot>::default() as Box<dyn Feature>,
                ),
                (
                    TypeId::of::<Ssh>(),
                    Box::new(Ssh::new(tx_reported_properties)) as Box<dyn Feature>,
                ),
                (
                    TypeId::of::<WifiCommissioning>(),
                    Box::<WifiCommissioning>::default() as Box<dyn Feature>,
                ),
            ]),
        }
    }

    pub async fn init_features(&mut self) -> Result<()> {
        dotenvy::from_path_override(Path::new(&format!(
            "{}/os-release",
            std::env::var("OS_RELEASE_DIR_PATH").unwrap_or_else(|_| "/etc".to_string())
        )))?;

        // report version
        self.tx_reported_properties
            .send(json!({
                "module-version": env!("CARGO_PKG_VERSION"),
                "azure-sdk-version": IotHubClient::sdk_version_string()
            }))
            .await?;

        for f in self.features.values_mut() {
            // report feature availability
            let value = if f.is_enabled() {
                json!({ "version": f.version() })
            } else {
                json!(null)
            };

            self.tx_reported_properties
                .send(json!({ f.name(): value }))
                .await?;
        }

        for f in self.features.values_mut() {
            if f.is_enabled() {
                // report initial feature status
                f.report_initial_state().await?;
            }
        }

        for f in self.features.values_mut() {
            if f.is_enabled() {
                // start feature
                f.start()?;
            }
        }

        Ok(())
    }

    fn feature<T>(&self) -> Result<&T>
    where
        T: Feature + 'static,
    {
        let feature = self
            .features
            .get(&TypeId::of::<T>())
            .ok_or_else(|| anyhow::anyhow!("failed to get feature"))?
            .as_any()
            .downcast_ref::<T>()
            .ok_or_else(|| anyhow::anyhow!("failed to cast to feature ref"))?;

        feature.ensure()?;

        Ok(feature)
    }

    fn feature_mut<T>(&mut self) -> Result<&mut T>
    where
        T: Feature + 'static,
    {
        let feature = self
            .features
            .get_mut(&TypeId::of::<T>())
            .ok_or_else(|| anyhow::anyhow!("failed to get feature mut"))?
            .as_any_mut()
            .downcast_mut::<T>()
            .ok_or_else(|| anyhow::anyhow!("failed to cast to feature mut"))?;

        feature.ensure()?;

        Ok(feature)
    }

    async fn handle_connection_status(&mut self, auth_status: AuthenticationStatus) -> Result<()> {
        info!("auth_status: {auth_status:#?}");

        match auth_status {
            AuthenticationStatus::Authenticated => {
                let mut update_validated: Result<()> = Ok(());

                if !self.authenticated_once {
                    /*
                     * the update validation test "wait_for_system_running" enforces that
                     * omnect-device-service already notified its own success
                     */

                    systemd::notify_ready();
                    update_validated = update_validation::check().await;

                    self.init_features().await?;

                    self.authenticated_once = true;
                };

                update_validated?;
            }
            AuthenticationStatus::Unauthenticated(reason) => {
                anyhow::ensure!(
                    matches!(reason, UnauthenticatedReason::ExpiredSasToken),
                    "No connection. Reason: {reason:?}"
                );
            }
        }

        Ok(())
    }

    async fn handle_desired(
        &mut self,
        state: TwinUpdateState,
        desired: serde_json::Value,
    ) -> Result<()> {
        info!("desired: {state:#?}, {desired}");

        match state {
            TwinUpdateState::Partial => {
                if let Some(gc) = desired.get("general_consent") {
                    self.feature::<DeviceUpdateConsent>()?
                        .update_general_consent(gc.as_array())
                        .await?;
                }

                if let Some(inf) = desired.get("include_network_filter") {
                    self.feature_mut::<NetworkStatus>()?
                        .update_include_network_filter(inf.as_array())
                        .await?;
                }
            }
            TwinUpdateState::Complete => {
                if desired.get("desired").is_none() {
                    bail!("handle_desired: 'desired' missing while TwinUpdateState::Complete")
                }

                self.feature::<DeviceUpdateConsent>()?
                    .update_general_consent(desired["desired"]["general_consent"].as_array())
                    .await?;

                self.feature_mut::<NetworkStatus>()?
                    .update_include_network_filter(
                        desired["desired"]["include_network_filter"].as_array(),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle_direct_method(
        &mut self,
        method_name: String,
        payload: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("handle_direct_method: {method_name} with payload: {payload}");

        match method_name.as_str() {
            "factory_reset" => {
                self.feature::<FactoryReset>()?
                    .reset_to_factory_settings(payload)
                    .await
            }
            "user_consent" => self.feature::<DeviceUpdateConsent>()?.user_consent(payload),
            "refresh_network_status" => {
                self.feature::<NetworkStatus>()?
                    .refresh_network_status()
                    .await
            }
            "refresh_ssh_status" => self.feature::<Ssh>()?.refresh_ssh_status().await,
            "open_ssh" => self.feature::<Ssh>()?.open_ssh(payload).await,
            "close_ssh" => self.feature::<Ssh>()?.close_ssh().await,
            "reboot" => self.feature::<Reboot>()?.reboot().await,
            _ => Err(anyhow!("direct method unknown")),
        }
    }

    async fn handle_report_property(&mut self, properties: serde_json::Value) -> Result<()> {
        info!("report: {properties}");

        match timeout(
            Duration::from_secs(5),
            self.iothub_client.twin_report(properties),
        )
        .await
        {
            Ok(result) => result.context("handle_report_property: couldn't report property"),
            Err(_) => Err(anyhow!("handle_report_property: timeout occurred")),
        }
    }

    pub async fn run(connection_string: Option<&str>) -> Result<()> {
        let (tx_connection_status, mut rx_connection_status) = mpsc::channel(100);
        let (tx_twin_desired, mut rx_twin_desired) = mpsc::channel(100);
        let (tx_direct_method, mut rx_direct_method) = mpsc::channel(100);
        let (tx_reported_properties, mut rx_reported_properties) = mpsc::channel(100);
        let mut sd_notify_interval = interval(Duration::from_secs(10));
        let mut wdt = WatchdogHandler::new();

        let client = match IotHubClient::client_type() {
            _ if connection_string.is_some() => IotHubClient::from_connection_string(
                connection_string.unwrap(),
                Some(tx_connection_status.clone()),
                Some(tx_twin_desired.clone()),
                Some(tx_direct_method.clone()),
                None,
            )?,
            ClientType::Device | ClientType::Module => {
                IotHubClient::from_identity_service(
                    Some(tx_connection_status.clone()),
                    Some(tx_twin_desired.clone()),
                    Some(tx_direct_method.clone()),
                    None,
                )
                .await?
            }
            ClientType::Edge => IotHubClient::from_edge_environment(
                Some(tx_connection_status.clone()),
                Some(tx_twin_desired.clone()),
                Some(tx_direct_method.clone()),
                None,
            )?,
        };

        let mut twin = Self::new(client, tx_reported_properties);

        loop {
            select! (
                _ = sd_notify_interval.tick() => {
                    wdt.notify()?;
                }
                status = rx_connection_status.recv() => {
                    twin.handle_connection_status(status.unwrap()).await?;
                },
                desired = rx_twin_desired.recv() => {
                    let (state, desired) = desired.unwrap();
                    twin.handle_desired(state, desired).await.unwrap_or_else(|e| error!("twin update desired properties: {e:#?}"));
                },
                reported = rx_reported_properties.recv()=> {
                    twin.handle_report_property(reported.unwrap()).await?;
                },
                direct_methods = rx_direct_method.recv() => {
                    /*
                        azure-iot-sdk-c calls direct method handler blocking in order to wait for the method result.
                        Since sdk uses only a single thread to handle all callbacks a deadlock might be the result,
                        if the method itself e.g. calls twin_report() which also blocks until the confirmation via
                        callback is received.
                        In order to workaround this issue we call now_or_never() which either returns Some(result) in case
                        future is ready or None otherwise. In the second case we assume the direct method succeeded and
                        has no result so we return Ok(None).
                    */
                    let (name, payload, tx_result) = direct_methods.unwrap();
                    let fut = twin.handle_direct_method(name, payload);
                    tokio::pin!(fut);
                    if let Some(result) = fut.as_mut().now_or_never() {
                        if tx_result.send(result).is_err() {
                            error!("run: receiver dropped");
                        }
                    } else {
                        if tx_result.send(Ok(None)).is_err() {
                            error!("run: receiver dropped");
                        }
                        if let Err(e) = fut.await {
                            error!("run: handle_direct_method: {e}");
                        }
                    };
                }
            );
        }
    }
}
