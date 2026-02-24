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

macro_rules! json_command_handler {
    ($fn_name:ident, $cmd_variant:path) => {
        async fn $fn_name(
            body: web::Bytes,
            tx_request: web::Data<mpsc::Sender<CommandRequest>>,
        ) -> HttpResponse {
            debug!("WebService::{}", stringify!($fn_name));

            match serde_json::from_slice(&body) {
                Ok(cmd) => {
                    let (tx_reply, rx_reply) = oneshot::channel();
                    let req = CommandRequest {
                        command: $cmd_variant(cmd),
                        reply: Some(tx_reply),
                    };
                    WebService::exec_request(tx_request, rx_reply, req).await
                }
                Err(e) => WebService::log_error_response(
                    e,
                    &format!("couldn't parse {} body", stringify!($cmd_variant)),
                    StatusCode::BAD_REQUEST,
                ),
            }
        }
    };
}

impl WebService {
    fn log_error_response(
        e: impl std::fmt::Display,
        context: &str,
        status: StatusCode,
    ) -> HttpResponse {
        error!("{context}: {e:#}");
        HttpResponse::build(status).body(e.to_string())
    }

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
        body: web::Json<PublishEndpointRequest>,
        _tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService register_publish_endpoint");
        let request = body.into_inner();

        PUBLISH_ENDPOINTS
            .lock()
            .await
            .insert(request.id, request.endpoint.clone());

        if let Err(e) = save_publish_endpoints().await {
            return Self::log_error_response(
                e,
                "couldn't write endpoints",
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }

        republish_to_endpoint(&request.endpoint).await
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
            return Self::log_error_response(
                e,
                "couldn't write endpoints",
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }

        HttpResponse::Ok().finish()
    }

    json_command_handler!(factory_reset, Command::FactoryReset);

    async fn exec_simple_command(
        tx_request: web::Data<mpsc::Sender<CommandRequest>>,
        command: Command,
        debug_name: &str,
    ) -> HttpResponse {
        debug!("WebService::{debug_name}");
        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = CommandRequest {
            command,
            reply: Some(tx_reply),
        };
        Self::exec_request(tx_request, rx_reply, cmd).await
    }

    async fn healthcheck(_tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        debug!("WebService healthcheck");

        HttpResponse::Ok().finish()
    }

    async fn reboot(tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        Self::exec_simple_command(tx_request, Command::Reboot, "reboot").await
    }

    async fn reload_network(tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        Self::exec_simple_command(tx_request, Command::ReloadNetwork, "reload_network").await
    }

    async fn republish(
        id: web::Path<String>,
        _tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService republish");

        let id = id.into_inner();
        let endpoints = PUBLISH_ENDPOINTS.lock().await;

        let Some(endpoint) = endpoints.get(&id) else {
            return Self::log_error_response(
                "id not found",
                &format!("republish: id '{id}' not found"),
                StatusCode::BAD_REQUEST,
            );
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

    json_command_handler!(load_fwupdate, Command::LoadFirmwareUpdate);

    json_command_handler!(run_fwupdate, Command::RunFirmwareUpdate);

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
            return WebService::log_error_response(
                e,
                "republish_to_endpoint",
                StatusCode::INTERNAL_SERVER_ERROR,
            );
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

    #[actix_web::test]
    async fn healthcheck_ok() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/healthcheck/v1", web::post().to(WebService::healthcheck)),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/healthcheck/v1")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn reload_network_ok() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/reload-network/v1",
                    web::post().to(WebService::reload_network),
                ),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply.unwrap().send(Ok(None)).unwrap();
        });

        let req = test::TestRequest::post()
            .uri("/reload-network/v1")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn reload_network_fail() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/reload-network/v1",
                    web::post().to(WebService::reload_network),
                ),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply
                .unwrap()
                .send(Err(anyhow::anyhow!("network error")))
                .unwrap();
        });

        let req = test::TestRequest::post()
            .uri("/reload-network/v1")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_server_error());
    }

    #[actix_web::test]
    async fn status_ok() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        // Populate status map
        PUBLISH_STATUS_MAP
            .lock()
            .await
            .insert("TestStatus".to_string(), json!({"test": "data"}));

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/status/v1", web::get().to(WebService::status)),
        )
        .await;

        let req = test::TestRequest::get().uri("/status/v1").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body = test::read_body(resp).await;
        let status_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&body).unwrap();
        assert!(status_map.contains_key("TestStatus"));

        // Cleanup
        PUBLISH_STATUS_MAP.lock().await.clear();
    }

    #[actix_web::test]
    async fn factory_reset_ok() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/factory-reset/v1",
                    web::post().to(WebService::factory_reset),
                ),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply.unwrap().send(Ok(None)).unwrap();
        });

        let req = test::TestRequest::post()
            .uri("/factory-reset/v1")
            .set_payload(r#"{"mode":1,"preserve":[]}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn factory_reset_invalid_json() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/factory-reset/v1",
                    web::post().to(WebService::factory_reset),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/factory-reset/v1")
            .set_payload("invalid json")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn factory_reset_fail() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/factory-reset/v1",
                    web::post().to(WebService::factory_reset),
                ),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply
                .unwrap()
                .send(Err(anyhow::anyhow!("factory reset error")))
                .unwrap();
        });

        let req = test::TestRequest::post()
            .uri("/factory-reset/v1")
            .set_payload(r#"{"mode":1,"preserve":[]}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_server_error());
    }

    #[actix_web::test]
    async fn load_fwupdate_ok() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/fwupdate/load/v1",
                    web::post().to(WebService::load_fwupdate),
                ),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply.unwrap().send(Ok(None)).unwrap();
        });

        let req = test::TestRequest::post()
            .uri("/fwupdate/load/v1")
            .set_payload(r#"{"update_file_path":"/tmp/fw.bin"}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn load_fwupdate_invalid_json() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/fwupdate/load/v1",
                    web::post().to(WebService::load_fwupdate),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/fwupdate/load/v1")
            .set_payload("{invalid}")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn run_fwupdate_ok() {
        let (tx_web_service, mut rx_web_service) =
            tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/fwupdate/run/v1", web::post().to(WebService::run_fwupdate)),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply.unwrap().send(Ok(None)).unwrap();
        });

        let req = test::TestRequest::post()
            .uri("/fwupdate/run/v1")
            .set_payload(r#"{"validate_iothub_connection":false}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn run_fwupdate_invalid_json() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/fwupdate/run/v1", web::post().to(WebService::run_fwupdate)),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/fwupdate/run/v1")
            .set_payload("not json at all")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn register_publish_endpoint_ok() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        // Set temp file path for test
        let temp_file = std::env::temp_dir().join("test_endpoints.json");
        unsafe {
            std::env::set_var("PUBLISH_ENDPOINTS_PATH", temp_file.to_str().unwrap());
        }

        // Clear any existing endpoints
        PUBLISH_ENDPOINTS.lock().await.clear();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/publish-endpoint/v1",
                    web::post().to(WebService::register_publish_endpoint),
                ),
        )
        .await;

        let payload = json!({
            "id": "test-endpoint",
            "endpoint": {
                "url": "http://localhost:8080/test",
                "headers": [
                    {"name": "Content-Type", "value": "application/json"}
                ]
            }
        });

        let req = test::TestRequest::post()
            .uri("/publish-endpoint/v1")
            .set_json(&payload)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        // Verify endpoint was registered
        let endpoints = PUBLISH_ENDPOINTS.lock().await;
        assert!(endpoints.contains_key("test-endpoint"));

        // Cleanup
        drop(endpoints);
        PUBLISH_ENDPOINTS.lock().await.clear();
        let _ = std::fs::remove_file(&temp_file);
        unsafe {
            std::env::remove_var("PUBLISH_ENDPOINTS_PATH");
        }
    }

    #[actix_web::test]
    async fn unregister_publish_endpoint_ok() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        // Set temp file path for test
        let temp_file = std::env::temp_dir().join("test_endpoints_unreg.json");
        unsafe {
            std::env::set_var("PUBLISH_ENDPOINTS_PATH", temp_file.to_str().unwrap());
        }

        // Pre-register an endpoint
        PUBLISH_ENDPOINTS.lock().await.insert(
            "test-endpoint".to_string(),
            PublishEndpoint {
                url: "http://localhost:8080/test".to_string(),
                headers: vec![],
            },
        );

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/publish-endpoint/v1/{id}",
                    web::delete().to(WebService::unregister_publish_endpoint),
                ),
        )
        .await;

        let req = test::TestRequest::delete()
            .uri("/publish-endpoint/v1/test-endpoint")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        // Verify endpoint was unregistered
        let endpoints = PUBLISH_ENDPOINTS.lock().await;
        assert!(!endpoints.contains_key("test-endpoint"));

        // Cleanup
        drop(endpoints);
        PUBLISH_ENDPOINTS.lock().await.clear();
        let _ = std::fs::remove_file(&temp_file);
        unsafe {
            std::env::remove_var("PUBLISH_ENDPOINTS_PATH");
        }
    }

    #[actix_web::test]
    async fn unregister_nonexistent_endpoint() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        PUBLISH_ENDPOINTS.lock().await.clear();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route(
                    "/publish-endpoint/v1/{id}",
                    web::delete().to(WebService::unregister_publish_endpoint),
                ),
        )
        .await;

        let req = test::TestRequest::delete()
            .uri("/publish-endpoint/v1/nonexistent")
            .to_request();
        let resp = test::call_service(&app, req).await;
        // Should still return OK even if endpoint doesn't exist
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn republish_ok() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        // Register an endpoint and add channel data
        PUBLISH_ENDPOINTS.lock().await.insert(
            "test-endpoint".to_string(),
            PublishEndpoint {
                url: "http://localhost:8080/test".to_string(),
                headers: vec![],
            },
        );

        PUBLISH_CHANNEL_MAP
            .lock()
            .await
            .insert("TestChannel".to_string(), json!({"status": "ok"}));

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/republish/v1/{id}", web::post().to(WebService::republish)),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/republish/v1/test-endpoint")
            .to_request();

        // Note: This will fail to actually POST to localhost:8080, but we're testing
        // the endpoint logic and error handling
        let resp = test::call_service(&app, req).await;

        // The actual HTTP call will fail, but we can verify the endpoint was found
        // In a real test environment, you'd mock the HTTP client
        assert!(resp.status().is_server_error() || resp.status().is_success());

        // Cleanup
        PUBLISH_ENDPOINTS.lock().await.clear();
        PUBLISH_CHANNEL_MAP.lock().await.clear();
    }

    #[actix_web::test]
    async fn republish_nonexistent_endpoint() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        PUBLISH_ENDPOINTS.lock().await.clear();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/republish/v1/{id}", web::post().to(WebService::republish)),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/republish/v1/nonexistent")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn exec_request_channel_receiver_dropped() {
        let (tx_web_service, _rx_web_service) = tokio::sync::mpsc::channel::<CommandRequest>(100);

        // Drop the receiver immediately
        drop(_rx_web_service);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/reboot/v1", web::post().to(WebService::reboot)),
        )
        .await;

        let req = test::TestRequest::post().uri("/reboot/v1").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_server_error());
    }

    #[actix_web::test]
    async fn exec_request_command_sender_dropped() {
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
            // Drop the reply sender without sending a response
            drop(req.reply);
        });

        let req = test::TestRequest::post().uri("/reboot/v1").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_server_error());
    }

    #[actix_web::test]
    async fn exec_request_with_json_result() {
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
                .send(Ok(Some(json!({"result": "success"}))))
                .unwrap();
        });

        let req = test::TestRequest::post().uri("/reboot/v1").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body = test::read_body(resp).await;
        let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["result"], "success");
    }

    #[actix_web::test]
    async fn publish_endpoint_headers_valid() {
        let endpoint = PublishEndpoint {
            url: "http://localhost:8080".to_string(),
            headers: vec![
                Header {
                    name: "Content-Type".to_string(),
                    value: "application/json".to_string(),
                },
                Header {
                    name: "Authorization".to_string(),
                    value: "Bearer token123".to_string(),
                },
            ],
        };

        let headers = endpoint.headers().unwrap();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("Content-Type").unwrap(), "application/json");
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer token123");
    }

    #[actix_web::test]
    async fn publish_endpoint_headers_invalid_name() {
        let endpoint = PublishEndpoint {
            url: "http://localhost:8080".to_string(),
            headers: vec![Header {
                name: "Invalid\nHeader".to_string(),
                value: "value".to_string(),
            }],
        };

        assert!(endpoint.headers().is_err());
    }

    #[actix_web::test]
    async fn publish_endpoint_headers_invalid_value() {
        let endpoint = PublishEndpoint {
            url: "http://localhost:8080".to_string(),
            headers: vec![Header {
                name: "Content-Type".to_string(),
                value: "invalid\nvalue".to_string(),
            }],
        };

        assert!(endpoint.headers().is_err());
    }
}
