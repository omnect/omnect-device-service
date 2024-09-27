mod consent;
mod factory_reset;
#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;
mod modem_info;
mod network_status;
mod provisioning_config;
mod reboot;
mod ssh_tunnel;
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
    network_status::NetworkStatus, provisioning_config::ProvisioningConfig, reboot::Reboot,
    ssh_tunnel::SshTunnel, wifi_commissioning::WifiCommissioning,
};
use crate::web_service::{self, Command as WebServiceCommand, PublishChannel, WebService};
use crate::{system, util};
use crate::{systemd, systemd::watchdog::WatchdogManager};
use crate::{update_validation, update_validation::UpdateValidation};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::{
    AuthenticationStatus, DirectMethod, IotMessage, TwinUpdateState, UnauthenticatedReason,
};
use dotenvy;
use enum_dispatch::enum_dispatch;
use futures_executor::block_on;
use futures_util::StreamExt;
use log::{error, info, warn};
use serde_json::json;
use signal_hook::consts::TERM_SIGNALS;
use signal_hook_tokio::Signals;
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    path::Path,
    time,
};
use strum_macros::EnumCount as EnumCountMacro;
use tokio::{select, sync::mpsc};

#[enum_dispatch]
#[derive(EnumCountMacro)]
enum TwinFeature {
    FactoryReset,
    DeviceUpdateConsent,
    ModemInfo,
    NetworkStatus,
    ProvisioningConfig,
    SshTunnel,
}

#[async_trait(?Send)]
#[enum_dispatch(TwinFeature)]
trait Feature {
    fn name(&self) -> String;

    fn version(&self) -> u8;

    fn is_enabled(&self) -> bool;

    fn as_any(&self) -> &dyn Any;

    fn ensure(&self) -> Result<()> {
        if !self.is_enabled() {
            bail!("feature disabled: {}", self.name());
        }

        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        unimplemented!();
    }

    async fn connect_twin(
        &mut self,
        _tx_reported_properties: mpsc::Sender<serde_json::Value>,
        _tx_outgoing_message: mpsc::Sender<IotMessage>,
    ) -> Result<()> {
        Ok(())
    }

    async fn connect_web_service(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(PartialEq)]
enum TwinState {
    Uninitialized,
    Initialized,
    Authenticated,
}

pub struct Twin {
    client: Option<IotHubClient>,
    web_service: Option<WebService>,
    tx_reported_properties: mpsc::Sender<serde_json::Value>,
    tx_outgoing_message: mpsc::Sender<IotMessage>,
    state: TwinState,
    features: HashMap<TypeId, Box<dyn Feature>>,
    update_validation: update_validation::UpdateValidation,
}

impl Twin {
    async fn new(
        tx_web_service: mpsc::Sender<WebServiceCommand>,
        tx_reported_properties: mpsc::Sender<serde_json::Value>,
        tx_outgoing_message: mpsc::Sender<IotMessage>,
    ) -> Result<Self> {
        // has to be called before iothub client authentication
        let update_validation = UpdateValidation::new()?;
        let client = None;
        let web_service = WebService::run(tx_web_service.clone()).await?;
        let state = TwinState::Uninitialized;

        let features = HashMap::from([
            (
                TypeId::of::<DeviceUpdateConsent>(),
                Box::<DeviceUpdateConsent>::default() as Box<dyn Feature>,
            ),
            (
                TypeId::of::<FactoryReset>(),
                Box::new(FactoryReset::new()) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<ModemInfo>(),
                Box::new(ModemInfo::new()) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<NetworkStatus>(),
                Box::new(NetworkStatus::new()?) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<ProvisioningConfig>(),
                Box::new(ProvisioningConfig::new()?) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<Reboot>(),
                Box::<Reboot>::default() as Box<dyn Feature>,
            ),
            (
                TypeId::of::<SshTunnel>(),
                Box::new(SshTunnel::new()) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<WifiCommissioning>(),
                Box::<WifiCommissioning>::default() as Box<dyn Feature>,
            ),
        ]);

        Ok(Twin {
            client,
            web_service,
            tx_reported_properties,
            tx_outgoing_message,
            state,
            features,
            update_validation,
        })
    }

    async fn connect_twin(&mut self) -> Result<()> {
        let client = self.client.as_ref().context("client not present")?;

        // report sdk versions
        client.twin_report(json!({
            "module-version": env!("CARGO_PKG_VERSION"),
            "azure-sdk-version": IotHubClient::sdk_version_string(),
        }))?;

        // report feature availability
        for f in self.features.values() {
            let value = if f.is_enabled() {
                json!({ "version": f.version() })
            } else {
                json!(null)
            };

            client.twin_report(json!({ f.name(): value }))?;
        }

        // connect twin channels
        for f in self.features.values_mut() {
            if f.is_enabled() {
                f.connect_twin(
                    self.tx_reported_properties.clone(),
                    self.tx_outgoing_message.clone(),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn connect_web_service(&self) -> Result<()> {
        web_service::publish(
            PublishChannel::Versions,
            json!({
                "os-version": system::sw_version()?,
                "azure-sdk-version": IotHubClient::sdk_version_string(),
                "omnect-device-service-version": env!("CARGO_PKG_VERSION"),
            }),
        )
        .await?;

        web_service::publish(PublishChannel::OnlineStatus, json!({"iothub": false})).await?;

        // connect twin channels
        for f in self.features.values() {
            if f.is_enabled() {
                f.connect_web_service().await?;
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

    fn handle_connection_status(&mut self, auth_status: AuthenticationStatus) -> Result<bool> {
        block_on(async {
            let mut restart_twin = false;
            match auth_status {
                AuthenticationStatus::Authenticated => {
                    if self.state != TwinState::Authenticated {
                        info!("Succeeded to connect to iothub");

                        if self.state == TwinState::Uninitialized {
                            /*
                             * the update validation test "wait_for_system_running" enforces that
                             * omnect-device-service already notified its own success
                             */
                            self.update_validation.set_authenticated().await?;
                        };

                        self.connect_twin().await?;

                        self.state = TwinState::Authenticated;
                    }
                }
                AuthenticationStatus::Unauthenticated(reason) => {
                    match reason {
                        UnauthenticatedReason::BadCredential
                        | UnauthenticatedReason::CommunicationError => {
                            error!("Failed to connect to iothub: {reason:?}. Possible reasons: certificate renewal, reprovisioning or wrong system time");

                            /*
                            here we start all over again. reason: there are situations where we get
                            a wrong connection string (BadCredential) from identity service due to wrong system time or
                            certificate renewal caused by iot-identity-service reprovisioning.
                            this may occur on devices without RTC or where time is not synced. since we experienced this
                            behavior only for a moment after boot (e.g. RPI without rtc) we just try again.
                            */
                            self.state = TwinState::Initialized;

                            restart_twin = true;
                        }
                        UnauthenticatedReason::RetryExpired
                        | UnauthenticatedReason::ExpiredSasToken
                        | UnauthenticatedReason::NoNetwork => {
                            info!("Failed to connect to iothub: {reason:?}")
                        }
                        UnauthenticatedReason::DeviceDisabled => {
                            warn!("Failed to connect to iothub: {reason:?}")
                        }
                    }
                }
            }

            web_service::publish(
                PublishChannel::OnlineStatus,
                json!({"iothub": auth_status == AuthenticationStatus::Authenticated}),
            )
            .await?;

            Ok(restart_twin)
        })
    }

    fn handle_desired(&self, state: TwinUpdateState, desired: serde_json::Value) -> Result<()> {
        block_on(async {
            info!("desired: {state:#?}, {desired}");

            match state {
                TwinUpdateState::Partial => {
                    if let Some(gc) = desired.get("general_consent") {
                        self.feature::<DeviceUpdateConsent>()?
                            .update_general_consent(gc.as_array())
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
                }
            }
            Ok(())
        })
    }

    fn handle_direct_method(&mut self, method: DirectMethod) -> Result<()> {
        block_on(async {
            let name = method.name.clone();
            let payload = method.payload.clone();

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
                "refresh_network_status" => self.feature_mut::<NetworkStatus>()?.refresh().await,
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
                "set_wait_online_timeout" => {
                    self.feature::<Reboot>()?
                        .set_wait_online_timeout(method.payload)
                        .await
                }
                _ => Err(anyhow!("direct method unknown")),
            };

            match &result {
                Ok(Some(result)) => info!("{name}({payload}) succeeded with result: {result}"),
                Ok(None) => info!("{name}({payload}) succeeded"),
                Err(e) => error!("{name}({payload}) returned error: {e}"),
            }

            if method.responder.send(result).is_err() {
                error!("handle_direct_method: {name}({payload}) receiver dropped");
            }

            Ok(())
        })
    }

    fn handle_webservice_request(&self, request: WebServiceCommand) -> Result<()> {
        block_on(async {
            let req_str = format!("{request:?}");

            let (tx_result, result) = match request {
                WebServiceCommand::FactoryReset(reply) => (
                    reply,
                    self.feature::<FactoryReset>()?
                        .reset_to_factory_settings(json!({"type": 1}))
                        .await
                        .map(|_| ()),
                ),
                WebServiceCommand::Reboot(reply) => (reply, systemd::reboot().await),
                WebServiceCommand::ReloadNetwork(reply) => (reply, system::reload_network().await),
            };

            match &result {
                Ok(()) => info!("handle_webservice_request: {req_str} succeeded"),
                Err(e) => error!("handle_webservice_request: {req_str} failed with: {e}"),
            }

            if tx_result.send(result.is_ok()).is_err() {
                error!("handle_webservice_request: receiver dropped");
            }

            result
        })
    }

    fn shutdown(
        &mut self,
        rx_reported_properties: &mut mpsc::Receiver<serde_json::Value>,
        rx_outgoing_message: &mut mpsc::Receiver<IotMessage>,
    ) {
        block_on(async {
            info!("shutdown");

            if let Some(client) = self.client.as_mut() {
                // report remaining properties
                while let Ok(reported) = rx_reported_properties.try_recv() {
                    client
                        .twin_report(reported)
                        .unwrap_or_else(|e| error!("couldn't report while shutting down: {e:#}"));
                }

                // send remaining messages
                while let Ok(message) = rx_outgoing_message.try_recv() {
                    client
                        .send_d2c_message(message)
                        .unwrap_or_else(|e| error!("couldn't send while shutting down: {e:#}"));
                }

                client.shutdown().await;
                self.client = None;
            }

            if let Some(ws) = &self.web_service {
                ws.shutdown().await;
            }
        })
    }

    fn reset_client_with_delay(&mut self, timeout: Option<time::Duration>) {
        block_on(async {
            if let Some(client) = self.client.as_mut() {
                info!("reset_client: shutdown iotclient");
                client.shutdown().await;
                self.client = None;
            }

            if let Some(t) = timeout {
                info!("reset_client: sleep for {}ms", t.as_millis());
                tokio::time::sleep(t).await
            }
        })
    }

    pub async fn run() -> Result<()> {
        let (tx_connection_status, mut rx_connection_status) = mpsc::channel(100);
        let (tx_twin_desired, mut rx_twin_desired) = mpsc::channel(100);
        let (tx_direct_method, mut rx_direct_method) = mpsc::channel(100);
        let (tx_reported_properties, mut rx_reported_properties) = mpsc::channel(100);
        let (tx_outgoing_message, mut rx_outgoing_message) = mpsc::channel(100);
        let (tx_web_service, mut rx_web_service) = mpsc::channel(100);

        // load env vars from /usr/lib/os-release, e.g. to determine feature availability
        dotenvy::from_path_override(Path::new(&format!(
            "{}/os-release",
            std::env::var("OS_RELEASE_DIR_PATH").unwrap_or_else(|_| "/usr/lib".to_string())
        )))?;

        let mut twin =
            Self::new(tx_web_service, tx_reported_properties, tx_outgoing_message).await?;

        let client_builder = IotHubClient::builder()
            .observe_connection_state(tx_connection_status)
            .observe_desired_properties(tx_twin_desired)
            .observe_direct_methods(tx_direct_method);

        twin.connect_web_service().await?;

        let mut signals = Signals::new(TERM_SIGNALS)?;

        tokio::pin! {
            let client_created = Self::connect_iothub_client(&client_builder);
            let trigger_watchdog = util::IntervalStream::new(WatchdogManager::init());
            let refresh_provisioning = util::IntervalStream::new(twin.feature::<ProvisioningConfig>()?.refresh_interval());
        };

        systemd::sd_notify_ready();

        loop {
            select! (
                // we enforce top down order in 1st select! macro to handle sd-notify, signals and connections status
                // with priority over events in the 2nd select!
                biased;

                _ = trigger_watchdog.next() => {
                    WatchdogManager::notify()?;
                },
                _ = signals.next() => {
                    twin.shutdown(&mut rx_reported_properties, &mut rx_outgoing_message);
                    signals.handle().close();
                    return Ok(())
                },
                result = &mut client_created, if twin.client.is_none() => {
                    match result {
                        Ok(client) => {
                            info!("iothub client created");
                            twin.client = Some(client);
                        },
                        Err(e) => {
                            error!("couldn't create iothub client: {e:#}");
                            twin.reset_client_with_delay(Some(time::Duration::from_secs(10)));
                            client_created.set(Self::connect_iothub_client(&client_builder));
                        }
                    }
                },
                Some(status) = rx_connection_status.recv() => {
                    if twin.handle_connection_status(status)?{
                        twin.reset_client_with_delay(Some(time::Duration::from_secs(1)));
                        client_created.set(Self::connect_iothub_client(&client_builder));
                    };
                },
                result = async {
                    select! (
                        // random access order in 2nd select! macro
                        Some(update_desired) = rx_twin_desired.recv() => {
                            twin.handle_desired(update_desired.state, update_desired.value)
                                .unwrap_or_else(|e| error!("handle desired properties: {e:#}"));
                        },
                        Some(reported) = rx_reported_properties.recv() => {
                            twin.client
                                .as_ref()
                                .context("couldn't report properties since client not present")?
                                .twin_report(reported)?
                        },
                        Some(direct_methods) = rx_direct_method.recv() => {
                            twin.handle_direct_method(direct_methods)?
                        },
                        Some(message) = rx_outgoing_message.recv() => {
                            twin.client
                                .as_ref()
                                .context("couldn't send msg since client not present")?
                                .send_d2c_message(message)?
                        },
                        Some(request) = rx_web_service.recv() => {
                            twin.handle_webservice_request(request)?
                        },
                        _ = refresh_provisioning.next() => {
                            block_on(twin.feature_mut::<ProvisioningConfig>()?.refresh())?;
                        },
                    );

                    Ok::<(), anyhow::Error>(())
                } => result?,
            );
        }
    }

    #[cfg(not(feature = "mock"))]
    async fn connect_iothub_client(builder: &IotHubClientBuilder) -> Result<IotHubClient> {
        info!("start client and wait for authentication...");

        builder.build_module_client_from_identity().await
    }

    #[cfg(feature = "mock")]
    async fn connect_iothub_client(builder: &IotHubClientBuilder) -> Result<IotHubClient> {
        info!("start client and wait for authentication...");

        builder.build_module_client(
            &std::env::var("CONNECTION_STRING").context("connection string missing")?,
        )
    }
}
