mod consent;
mod factory_reset;
#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;
mod modem_info;
mod network_status;
mod reboot;
mod ssh_tunnel;
mod web_service;
mod wifi_commissioning;

cfg_if::cfg_if! {
    if #[cfg(test)] {
        use mod_test::mod_test::MockMyIotHub as IotHubClient;
        use mod_test::mod_test::MyIotHubBuilder as IotHubClientBuilder;
    } else {
        use azure_iot_sdk::client::{IotHubClient, IotHubClientBuilder};
    }
}

use crate::twin::{
    consent::DeviceUpdateConsent, factory_reset::FactoryReset, modem_info::ModemInfo,
    network_status::NetworkStatus, reboot::Reboot, ssh_tunnel::SshTunnel, web_service::WebService,
    wifi_commissioning::WifiCommissioning,
};
use crate::update_validation::UpdateValidation;
use crate::{system, update_validation};
use crate::{systemd, systemd::watchdog::WatchdogManager};

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::{
    AuthenticationStatus, DirectMethod, IotMessage, TwinUpdate, TwinUpdateState,
    UnauthenticatedReason,
};
use dotenvy;
use enum_dispatch::enum_dispatch;
use futures_util::{FutureExt, StreamExt};
use log::{debug, error, info, warn};
use serde_json::json;
use signal_hook::consts::TERM_SIGNALS;
use signal_hook_tokio::Signals;
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    future::{pending, Future},
    path::Path,
};
use strum_macros::EnumCount as EnumCountMacro;
use tokio::{
    select,
    sync::mpsc,
    time::{interval, Interval},
};

#[enum_dispatch]
#[derive(EnumCountMacro)]
enum TwinFeature {
    FactoryReset,
    DeviceUpdateConsent,
    ModemInfo,
    NetworkStatus,
    SshTunnel,
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
    client: IotHubClient,
    client_builder: IotHubClientBuilder,
    authenticated_once: bool,
    rx_connection_status: mpsc::Receiver<AuthenticationStatus>,
    rx_twin_desired: mpsc::Receiver<TwinUpdate>,
    rx_direct_method: mpsc::Receiver<DirectMethod>,
    rx_reported_properties: mpsc::Receiver<serde_json::Value>,
    rx_outgoing_message: mpsc::Receiver<IotMessage>,
    features: HashMap<TypeId, Box<dyn Feature>>,
    update_validation: update_validation::UpdateValidation,
}

impl Twin {
    async fn new() -> Result<Self> {
        let (tx_connection_status, rx_connection_status) = mpsc::channel(100);
        let (tx_twin_desired, rx_twin_desired) = mpsc::channel(100);
        let (tx_direct_method, rx_direct_method) = mpsc::channel(100);
        let (tx_reported_properties, rx_reported_properties) = mpsc::channel(100);
        let (tx_outgoing_message, rx_outgoing_message) = mpsc::channel(100);

        // has to be called before iothub client authentication
        let update_validation = UpdateValidation::new()?;

        let client_builder = IotHubClient::builder()
            .observe_connection_state(tx_connection_status)
            .observe_desired_properties(tx_twin_desired)
            .observe_direct_methods(tx_direct_method)
            .pnp_model_id("dtmi:azure:iot:deviceUpdateModel;3");

        let client = Self::build_twin(&client_builder).await?;

        let features = HashMap::from([
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
                TypeId::of::<ModemInfo>(),
                Box::new(ModemInfo::new(tx_reported_properties.clone())) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<NetworkStatus>(),
                Box::new(NetworkStatus::new(tx_reported_properties.clone())) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<Reboot>(),
                Box::<Reboot>::default() as Box<dyn Feature>,
            ),
            (
                TypeId::of::<SshTunnel>(),
                Box::new(SshTunnel::new(tx_outgoing_message)) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<WifiCommissioning>(),
                Box::<WifiCommissioning>::default() as Box<dyn Feature>,
            ),
        ]);

        Ok(Twin {
            client,
            client_builder,
            rx_connection_status,
            rx_twin_desired,
            rx_direct_method,
            rx_reported_properties,
            rx_outgoing_message,
            authenticated_once: false,
            features,
            update_validation,
        })
    }

    async fn init(&mut self) -> Result<()> {
        dotenvy::from_path_override(Path::new(&format!(
            "{}/os-release",
            std::env::var("OS_RELEASE_DIR_PATH").unwrap_or_else(|_| "/etc".to_string())
        )))?;

        // report version
        self.client.twin_report(json!({
            "module-version": env!("CARGO_PKG_VERSION"),
            "azure-sdk-version": IotHubClient::sdk_version_string()
        }))?;

        for f in self.features.values_mut() {
            // report feature availability
            let value = if f.is_enabled() {
                json!({ "version": f.version() })
            } else {
                json!(null)
            };

            self.client.twin_report(json!({ f.name(): value }))?;
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
        match auth_status {
            AuthenticationStatus::Authenticated => {
                if !self.authenticated_once {
                    info!("Succeeded to connect to iothub");

                    /*
                     * the update validation test "wait_for_system_running" enforces that
                     * omnect-device-service already notified its own success
                     */
                    self.update_validation.set_authenticated().await?;

                    self.init().await?;

                    self.authenticated_once = true;
                };
            }
            AuthenticationStatus::Unauthenticated(reason) => match reason {
                UnauthenticatedReason::BadCredential
                | UnauthenticatedReason::CommunicationError => {
                    error!("Failed to connect to iothub: {reason:?}");

                    /*
                        here we start all over again. reason: there are situations where we get
                        a wrong connection string (BadCredential) from identity service due to wrong system time.
                        this may occur on devices without RTC or where time is not synced. since we experienced this
                        behavior only for a moment after boot (e.g. RPI without rtc) we just try again.
                     */
                    self.client = Self::build_twin(&self.client_builder).await?;
                }
                UnauthenticatedReason::RetryExpired
                | UnauthenticatedReason::ExpiredSasToken
                | UnauthenticatedReason::NoNetwork => {
                    info!("Failed to connect to iothub: {reason:?}")
                }
                UnauthenticatedReason::DeviceDisabled => {
                    warn!("Failed to connect to iothub: {reason:?}")
                }
            },
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

    async fn handle_direct_method(&self, method: DirectMethod) -> Result<()> {
        info!(
            "handle_direct_method: {} with payload: {}",
            method.name, method.payload
        );

        let result = match method.name.as_str() {
            "factory_reset" => {
                self.feature::<FactoryReset>()?
                    .reset_to_factory_settings(method.payload)
                    .await
            }
            "user_consent" => self
                .feature::<DeviceUpdateConsent>()?
                .user_consent(method.payload),
            "refresh_modem_info" => self.feature::<ModemInfo>()?.refresh_modem_info().await,
            "refresh_network_status" => {
                self.feature::<NetworkStatus>()?
                    .refresh_network_status()
                    .await
            }
            "get_ssh_pub_key" => {
                self.feature::<SshTunnel>()?
                    .get_ssh_pub_key(method.payload)
                    .await
            }
            "open_ssh_tunnel" => {
                self.feature::<SshTunnel>()?
                    .open_ssh_tunnel(method.payload)
                    .await
            }
            "close_ssh_tunnel" => {
                self.feature::<SshTunnel>()?
                    .close_ssh_tunnel(method.payload)
                    .await
            }
            "reboot" => self.feature::<Reboot>()?.reboot().await,
            _ => Err(anyhow!("direct method unknown")),
        };

        if method.responder.send(result).is_err() {
            error!("handle_direct_method: mpsc::Receiver dropped");
        }

        Ok(())
    }

    async fn handle_web_service_request(&self, request: web_service::Command) -> Result<()> {
        info!("handle_web_service_request: {:?}", request);

        let (tx_result, result) = match request {
            web_service::Command::GetOsVersion(reply) => {
                (reply, json!({"version": system::sw_version()?}))
            }
            web_service::Command::Reboot(reply) => {
                (reply, json!({"result": systemd::reboot().await.is_ok()}))
            }
            web_service::Command::ReloadNetwork(reply) => (
                reply,
                json!({"result": system::reload_network().await.is_ok()}),
            ),
        };

        if tx_result.send(result).is_err() {
            error!("handle_web_service_request: mpsc::Receiver dropped");
        }

        Ok(())
    }

    pub async fn run() -> Result<()> {
        let (tx_web_service, mut rx_web_service) = mpsc::channel(100);
        let mut signals = Signals::new(TERM_SIGNALS)?;
        let mut sd_notify_interval = None;

        if let Some(timeout) = WatchdogManager::init() {
            let timeout = timeout / 2;
            debug!("trigger watchdog interval: {}µs", timeout.as_micros());
            sd_notify_interval = Some(interval(timeout));
        };

        let mut twin = Self::new().await?;

        let web_service = WebService::new(tx_web_service.clone())?;

        systemd::sd_notify_ready();

        loop {
            select! (
                _ =  Self::notify_some_interval(&mut sd_notify_interval) => {
                    WatchdogManager::notify()?;
                },
                _ = signals.next() => {
                    signals.handle().close();
                    web_service.shutdown().await;
                    twin.client.shutdown().await;
                    return Ok(())
                },
                Some(status) = twin.rx_connection_status.recv() => {
                    twin.handle_connection_status(status).await?;
                },
                Some(update_desired) = twin.rx_twin_desired.recv() => {
                    twin.handle_desired(update_desired.state, update_desired.value)
                        .await
                        .unwrap_or_else(|e| error!("twin update desired properties: {e:#}"));
                },
                Some(reported) = twin.rx_reported_properties.recv() => {
                    twin.client.twin_report(reported)?
                },
                Some(direct_methods) = twin.rx_direct_method.recv() => {
                    twin.handle_direct_method(direct_methods).await?
                },
                Some(message) = twin.rx_outgoing_message.recv() => {
                    twin.client.send_d2c_message(message)?
                },
                Some(request) = rx_web_service.recv() => {
                    twin.handle_web_service_request(request).await?
                }
            );
        }
    }

    #[cfg(not(feature = "mock"))]
    async fn build_twin(builder: &IotHubClientBuilder) -> Result<IotHubClient> {
        info!("start client and wait for authentication...");

        builder.build_module_client_from_identity().await
    }

    #[cfg(feature = "mock")]
    async fn build_twin(builder: &IotHubClientBuilder) -> Result<IotHubClient> {
        info!("start client and wait for authentication...");

        builder.build_module_client(&std::env::var("CONNECTION_STRING")?)
    }

    fn notify_some_interval(
        interval: &mut Option<Interval>,
    ) -> impl Future<Output = tokio::time::Instant> + '_ {
        match interval.as_mut() {
            Some(i) => i.tick().left_future(),
            None => pending().right_future(),
        }
    }
}
