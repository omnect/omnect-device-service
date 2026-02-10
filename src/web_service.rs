use crate::{
    common::{from_json_file, to_json_file},
    twin::feature::*,
};
use actix_server::ServerHandle;
use actix_web::{App, HttpResponse, HttpServer, http::StatusCode, web};
use anyhow::{Context, Result};
use log::{debug, error, info};
use reqwest::{Client, header};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    env,
    path::Path,
    str::FromStr,
    sync::{LazyLock, OnceLock},
};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time::Duration,
};

static SHUTDWOWN_TIMEOUT_SECS: u64 = 10;
static IS_WEBSERVICE_DISABLED: OnceLock<bool> = OnceLock::new();
static PUBLISH_CHANNEL_MAP: LazyLock<Mutex<serde_json::Map<String, serde_json::Value>>> =
    LazyLock::new(|| Mutex::new(serde_json::Map::default()));
static PUBLISH_STATUS_MAP: LazyLock<Mutex<serde_json::Map<String, serde_json::Value>>> =
    LazyLock::new(|| Mutex::new(serde_json::Map::default()));
static PUBLISH_ENDPOINTS: LazyLock<Mutex<HashMap<String, PublishEndpoint>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PUBLISH_CLIENT: LazyLock<Mutex<ClientWithMiddleware>> = LazyLock::new(|| {
    Mutex::new(
        ClientBuilder::new(
            Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("building ClientWithMiddleware failed"),
        )
        .with(RetryTransientMiddleware::new_with_policy(
            ExponentialBackoff::builder().build_with_total_retry_duration(Duration::from_secs(15)),
        ))
        .build(),
    )
});

macro_rules! publish_endpoints_path {
    () => {
        env::var("PUBLISH_ENDPOINTS_PATH")
            .unwrap_or("/run/omnect-device-service/publish_endpoints.json".to_string())
    };
}

#[derive(Debug, strum_macros::Display)]
pub enum PublishChannel {
    FactoryResetV1,
    NetworkStatusV1,
    OnlineStatusV1,
    SystemInfoV1,
    TimeoutsV1,
    UpdateValidationStatusV1,
}

#[cfg(not(test))]
impl PublishChannel {
    fn to_status_string(&self) -> String {
        match self {
            PublishChannel::FactoryResetV1 => "FactoryResetResult".to_string(),
            PublishChannel::NetworkStatusV1 => "NetworkStatus".to_string(),
            PublishChannel::OnlineStatusV1 => "OnlineStatus".to_string(),
            PublishChannel::SystemInfoV1 => "SystemInfo".to_string(),
            PublishChannel::TimeoutsV1 => "Timeouts".to_string(),
            PublishChannel::UpdateValidationStatusV1 => "UpdateValidationStatus".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Header {
    name: String,
    value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublishEndpoint {
    url: String,
    headers: Vec<Header>,
}

impl PublishEndpoint {
    fn headers(&self) -> Result<header::HeaderMap> {
        let mut headers = header::HeaderMap::new();

        for h in &self.headers {
            headers.insert(
                header::HeaderName::from_str(&h.name).context("failed to get header name")?,
                header::HeaderValue::from_str(&h.value).context("failed to get header value")?,
            );
        }

        Ok(headers)
    }
}

#[derive(Debug, Deserialize)]
struct PublishEndpointRequest {
    id: String,
    endpoint: PublishEndpoint,
}

pub struct WebService {
    srv_handle: ServerHandle,
}

impl WebService {
    pub async fn run(tx_request: mpsc::Sender<CommandRequest>) -> Result<Option<Self>> {
        // we only start web service feature if not explicitly disabled by 'DISABLE_WEBSERVICE="true"' env var
        if *IS_WEBSERVICE_DISABLED.get_or_init(|| {
            env::var("DISABLE_WEBSERVICE")
                .unwrap_or("false".to_string())
                .to_lowercase()
                == "true"
        }) {
            info!("WebService is disabled");
            return Ok(None);
        };

        info!("WebService is enabled");

        if matches!(Path::new(&publish_endpoints_path!()).try_exists(), Ok(true)) {
            debug!("restore publish endpoints");
            *PUBLISH_ENDPOINTS.lock().await = from_json_file(publish_endpoints_path!())?;
        }

        let srv = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(tx_request.clone()))
                .route(
                    "/publish-endpoint/v1",
                    web::post().to(Self::register_publish_endpoint),
                )
                .route(
                    "/publish-endpoint/v1/{id}",
                    web::delete().to(Self::unregister_publish_endpoint),
                )
                .route("/factory-reset/v1", web::post().to(Self::factory_reset))
                .route("/fwupdate/load/v1", web::post().to(Self::load_fwupdate))
                .route("/fwupdate/run/v1", web::post().to(Self::run_fwupdate))
                .route("/healthcheck/v1", web::post().to(Self::healthcheck))
                .route("/reboot/v1", web::post().to(Self::reboot))
                .route("/reload-network/v1", web::post().to(Self::reload_network))
                .route("/republish/v1/{id}", web::post().to(Self::republish))
                .route("/status/v1", web::get().to(Self::status))
        });

        let srv = if cfg!(feature = "mock") {
            const SOCKET_PATH: &str = "/tmp/api.sock";

            debug!("bind to {SOCKET_PATH}");

            srv.bind_uds(SOCKET_PATH)
                .context(format!("web_service: cannot bind to {SOCKET_PATH}"))?
        } else {
            /*
                We expect an existing unix socket created by systemd/omnect-device-service.socket
                This socket must have the socket file descriptor "3". It would be possible to generically
                determine the index by socket path, but there is no need yet.
                An example for a complete generic handling can be found in iot-identity-service code:
                https://github.com/Azure/iot-identity-service/blob/main/http-common/src/connector.rs

                The socket is not reliable removed by systemd when the service stops, crashes or returns with an error.
                As a result sometimes files or directories with wrong permissions are created. Thus we cleanup socket in
                omnect-device-service.socket "ExecStartPre" and "ExecStopPost".
            */
            const SOCKET_FDS_IDX: std::os::unix::io::RawFd = 3;

            let listener: std::os::unix::net::UnixListener =
                unsafe { std::os::unix::io::FromRawFd::from_raw_fd(SOCKET_FDS_IDX) };

            debug!("listen to {:?}", listener.local_addr().unwrap());

            listener
                .set_nonblocking(true)
                .context("web_service: cannot create UnixListener")?;

            srv.listen_uds(listener)
                .context("web_service: cannot listen on UnixListener")?
        };

        let srv = srv
            .disable_signals()
            .shutdown_timeout(SHUTDWOWN_TIMEOUT_SECS)
            .run();

        let srv_handle = srv.handle();

        tokio::spawn(srv);

        Ok(Some(WebService { srv_handle }))
    }

    pub async fn shutdown(&self) {
        debug!("WebService shutdown");

        self.srv_handle.stop(false).await;

        debug!("WebService shutdown complete");
    }

    async fn register_publish_endpoint(
        body: web::Bytes,
        _tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService register_publish_endpoint");

        match serde_json::from_slice::<PublishEndpointRequest>(&body) {
            Ok(request) => {
                PUBLISH_ENDPOINTS
                    .lock()
                    .await
                    .insert(request.id, request.endpoint.clone());

                if let Err(e) = save_publish_endpoints().await {
                    error!("couldn't write endpoints: {e:#}");
                    return HttpResponse::InternalServerError().body(e.to_string());
                }

                republish_to_endpoint(&request.endpoint).await
            }
            Err(e) => {
                error!("couldn't parse PublishEndpointRequest from body: {e:#}");
                HttpResponse::BadRequest().body(e.to_string())
            }
        }
    }

    async fn unregister_publish_endpoint(
        id: web::Path<String>,
        _tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService unregister_publish_endpoint");

        if PUBLISH_ENDPOINTS
            .lock()
            .await
            .remove(&id.into_inner())
            .is_some()
            && let Err(e) = save_publish_endpoints().await
        {
            error!("couldn't write endpoints: {e:#}");
            return HttpResponse::InternalServerError().body(e.to_string());
        }

        HttpResponse::Ok().finish()
    }

    async fn factory_reset(
        body: web::Bytes,
        tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService factory_reset");

        match serde_json::from_slice(&body) {
            Ok(command) => {
                let (tx_reply, rx_reply) = oneshot::channel();
                let req = CommandRequest {
                    command: Command::FactoryReset(command),
                    reply: Some(tx_reply),
                };

                Self::exec_request(tx_request, rx_reply, req).await
            }
            Err(e) => {
                error!("couldn't parse FactoryResetCommand from body: {e:#}");
                HttpResponse::BadRequest().body(e.to_string())
            }
        }
    }

    async fn healthcheck(_tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        debug!("WebService healthcheck");

        HttpResponse::Ok().finish()
    }

    async fn reboot(tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        debug!("WebService reboot");

        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = CommandRequest {
            command: Command::Reboot,
            reply: Some(tx_reply),
        };

        Self::exec_request(tx_request, rx_reply, cmd).await
    }

    async fn reload_network(tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        debug!("WebService reload_network");

        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = CommandRequest {
            command: Command::ReloadNetwork,
            reply: Some(tx_reply),
        };

        Self::exec_request(tx_request, rx_reply, cmd).await
    }

    async fn republish(
        id: web::Path<String>,
        _tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService republish");

        let id = id.into_inner();
        let endpoints = PUBLISH_ENDPOINTS.lock().await;

        let Some(endpoint) = endpoints.get(&id) else {
            error!("republish: id '{id}' not found");
            return HttpResponse::BadRequest().body("id not found");
        };

        republish_to_endpoint(endpoint).await
    }

    async fn status(_tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        debug!("WebService status");

        let pubs = PUBLISH_STATUS_MAP.lock().await;

        HttpResponse::Ok().body(
            serde_json::to_string(&pubs.clone()).expect("cannot convert publish map to string"),
        )
    }

    async fn load_fwupdate(
        body: web::Bytes,
        tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService load_fwupdate");

        match serde_json::from_slice(&body) {
            Ok(command) => {
                let (tx_reply, rx_reply) = oneshot::channel();
                let req = CommandRequest {
                    command: Command::LoadFirmwareUpdate(command),
                    reply: Some(tx_reply),
                };

                Self::exec_request(tx_request, rx_reply, req).await
            }
            Err(e) => {
                error!("couldn't parse LoadFirmwareUpdate from body: {e:#}");
                HttpResponse::BadRequest().body(e.to_string())
            }
        }
    }

    async fn run_fwupdate(
        body: web::Bytes,
        tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService run_fwupdate");

        match serde_json::from_slice(&body) {
            Ok(command) => {
                let (tx_reply, rx_reply) = oneshot::channel();
                let req = CommandRequest {
                    command: Command::RunFirmwareUpdate(command),
                    reply: Some(tx_reply),
                };

                Self::exec_request(tx_request, rx_reply, req).await
            }
            Err(e) => {
                error!("couldn't parse RunFirmwareUpdate from body: {e:#}");
                HttpResponse::BadRequest().body(e.to_string())
            }
        }
    }

    async fn exec_request(
        tx_request: web::Data<mpsc::Sender<CommandRequest>>,
        rx_reply: tokio::sync::oneshot::Receiver<CommandResult>,
        request: CommandRequest,
    ) -> HttpResponse {
        info!("execute request: send {request:?}");

        if tx_request.send(request).await.is_err() {
            error!("execute request: command receiver dropped");
            return HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR).finish();
        }

        let Ok(result) = rx_reply.await else {
            error!("execute request: command sender dropped");
            return HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR).finish();
        };

        match result {
            Ok(Some(content)) => {
                info!("execute request: succeeded with result {content:?}");
                HttpResponse::Ok().json(content)
            }
            Ok(None) => {
                info!("execute request: succeeded");
                HttpResponse::Ok().finish()
            }
            Err(e) => {
                error!("execute request: request failed with: {e:#}");
                HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR).body(e.to_string())
            }
        }
    }
}

#[cfg(not(test))]
pub async fn publish(channel: PublishChannel, value: serde_json::Value) {
    if *IS_WEBSERVICE_DISABLED.wait() {
        debug!("publish: skip since feature not enabled");
        return;
    }

    debug!("publish");

    let msg = json!({"channel": channel.to_string(), "data": value}).to_string();

    PUBLISH_CHANNEL_MAP
        .lock()
        .await
        .insert(channel.to_string(), value.clone());

    PUBLISH_STATUS_MAP
        .lock()
        .await
        .insert(channel.to_status_string(), value.clone());

    for endpoint in PUBLISH_ENDPOINTS.lock().await.values() {
        let msg = msg.clone();
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            if let Err(e) = publish_to_endpoint(msg, &endpoint).await {
                error!("publish: {e:#}");
            }
        });
    }
}

#[cfg(test)]
pub async fn publish(_channel: PublishChannel, _value: serde_json::Value) {}

async fn republish_to_endpoint(endpoint: &PublishEndpoint) -> HttpResponse {
    for (channel, value) in PUBLISH_CHANNEL_MAP.lock().await.iter() {
        let msg = json!({"channel": channel, "data": value}).to_string();

        if let Err(e) = publish_to_endpoint(msg, endpoint).await {
            error!("republish_to_endpoint: {e:#}");
            return HttpResponse::InternalServerError().body(e.to_string());
        }
    }

    HttpResponse::Ok().finish()
}

async fn publish_to_endpoint(msg: String, endpoint: &PublishEndpoint) -> Result<reqwest::Response> {
    Ok(PUBLISH_CLIENT
        .lock()
        .await
        .post(&endpoint.url)
        .headers(endpoint.headers()?)
        .body(msg)
        .send()
        .await
        .context("publish_to_endpoint")?
        .error_for_status()?)
}

async fn save_publish_endpoints() -> Result<()> {
    to_json_file(
        &*PUBLISH_ENDPOINTS.lock().await,
        publish_endpoints_path!(),
        true,
    )
}

#[cfg(test)]
mod tests {
    use actix_web::{App, test};

    use super::*;

    #[actix_web::test]
    async fn reboot_ok() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/reboot/v1", web::post().to(WebService::reboot)),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply.unwrap().send(Ok(None)).unwrap();
        });

        let req = test::TestRequest::post().uri("/reboot/v1").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn reboot_fail() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/reboot/v1", web::post().to(WebService::reboot)),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply
                .unwrap()
                .send(Err(anyhow::anyhow!("error")))
                .unwrap();
        });

        let req = test::TestRequest::post().uri("/reboot/v1").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_server_error());
    }

    /*
               .route("/reboot/v1", web::post().to(Self::reboot))
               .route("/reload-network/v1", web::post().to(Self::reload_network))
               .route("/republish/v1", web::post().to(Self::republish))
               .route("/status/v1", web::get().to(Self::status))
    */
}
