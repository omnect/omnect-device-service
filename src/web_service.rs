use crate::twin::feature::*;
use actix_server::ServerHandle;
use actix_web::{http::StatusCode, web, App, HttpResponse, HttpServer};
use anyhow::{bail, Context, Result};
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, fs::OpenOptions, str::FromStr, sync::LazyLock};
use tokio::sync::{mpsc, oneshot, Mutex};

static PUBLISH_CHANNEL_MAP: LazyLock<Mutex<serde_json::Map<String, serde_json::Value>>> =
    LazyLock::new(|| Mutex::new(serde_json::Map::default()));
static PUBLISH_ENDPOINTS: LazyLock<Mutex<HashMap<String, PublishEndpoint>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

macro_rules! centrifugo_endpoints_path {
    () => {{
        static CENTRIFUGO_ENDPOINTS_PATH_DEFAULT: &'static str =
            "/var/lib/omnect-device-service/publish_endpoints";
        std::env::var("CENTRIFUGO_ENDPOINTS_PATH")
            .unwrap_or(CENTRIFUGO_ENDPOINTS_PATH_DEFAULT.to_string())
    }};
}

#[derive(Debug, strum_macros::Display)]
pub enum PublishChannel {
    FactoryResetKeys,
    FactoryResetStatus,
    NetworkStatus,
    OnlineStatus,
    SystemInfo,
    Timeouts,
    UpdateValidationStatus,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Header {
    name: String,
    value: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PublishEndpoint {
    url: String,
    headers: Vec<Header>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PublishEndpointRequest {
    id: String,
    endpoint: PublishEndpoint,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PublishEndpointAppId {
    id: String,
}

lazy_static! {
    static ref IS_WEBSERVICE_ENABLED: bool = {
        std::env::var("DISABLE_WEBSERVICE")
            .unwrap_or("false".to_string())
            .to_lowercase()
            != "true"
    };
}

pub struct WebService {
    srv_handle: ServerHandle,
}

impl WebService {
    pub async fn run(tx_request: mpsc::Sender<CommandRequest>) -> Result<Option<Self>> {
        // we only start web service feature if not explicitly disabled by 'DISABLE_WEBSERVICE="true"' env var
        if !(*IS_WEBSERVICE_ENABLED) {
            info!("WebService is disabled");
            return Ok(None);
        };

        info!("WebService is enabled");

        let srv = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(tx_request.clone()))
                .route(
                    "/publish-endpoint/v1",
                    web::post().to(Self::set_publish_endpoint),
                )
                .route(
                    "/publish-endpoint/v1/{id}",
                    web::delete().to(Self::delete_publish_endpoint),
                )
                .route("/factory-reset/v1", web::post().to(Self::factory_reset))
                .route("/fwupdate/load/v1", web::post().to(Self::load_fwupdate))
                .route("/fwupdate/run/v1", web::post().to(Self::run_fwupdate))
                .route("/reboot/v1", web::post().to(Self::reboot))
                .route("/reload-network/v1", web::post().to(Self::reload_network))
                .route("/republish/v1", web::post().to(Self::republish))
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

        let srv = srv.run();

        let srv_handle = srv.handle();

        tokio::spawn(srv);

        Ok(Some(WebService { srv_handle }))
    }

    pub async fn shutdown(&self) {
        debug!("WebService shutdown");

        self.srv_handle.stop(false).await;
    }

    async fn set_publish_endpoint(
        body: web::Bytes,
        _tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService set_publish_endpoint");

        match serde_json::from_slice::<PublishEndpointRequest>(&body) {
            Ok(request) => {
                // Read the existing JSON file, or create an empty array if it doesn't exist
                let mut endpoints: Vec<PublishEndpointRequest> =
                    match std::fs::read_to_string(centrifugo_endpoints_path!()) {
                        Ok(content) => {
                            serde_json::from_str(&content).unwrap_or_else(|_| Vec::new())
                        }
                        Err(_) => Vec::new(),
                    };
                // Check if the id exists, update if found, else push new
                let mut found = false;
                for endpoint in endpoints.iter_mut() {
                    if endpoint.id == request.id {
                        *endpoint = request.clone();
                        found = true;
                        break;
                    }
                }
                if !found {
                    endpoints.push(request);
                }

                let _ = serde_json::to_writer_pretty(
                    OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(centrifugo_endpoints_path!())
                        .unwrap(),
                    &endpoints,
                )
                .context("web_service: serde_json::to_writer_pretty");

                HttpResponse::Ok().finish()
            }
            Err(e) => {
                error!("couldn't parse PublishEndpointRequest from body: {e:#}");
                HttpResponse::BadRequest().body(e.to_string())
            }
        }
    }

    async fn delete_publish_endpoint(
        path: web::Path<PublishEndpointAppId>,
        _tx_request: web::Data<mpsc::Sender<CommandRequest>>,
    ) -> HttpResponse {
        debug!("WebService delete_publish_endpoint");

        let mut endpoints: Vec<PublishEndpointRequest> =
            match std::fs::read_to_string(centrifugo_endpoints_path!()) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| Vec::new()),
                Err(_) => Vec::new(),
            };
        endpoints.retain(|client| client.id != path.id);

        let _ = serde_json::to_writer_pretty(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(centrifugo_endpoints_path!())
                .unwrap(),
            &endpoints,
        )
        .context("web_service: serde_json::to_writer_pretty");

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

    async fn republish(_tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        debug!("WebService republish");

        for (channel, value) in PUBLISH_CHANNEL_MAP.lock().await.iter() {
            let msg = json!({"channel": channel, "data": value});

            if let Err(e) = publish_to_endpoints(msg).await {
                return HttpResponse::InternalServerError().body(e.to_string());
            }
        }

        HttpResponse::Ok().finish()
    }

    async fn status(_tx_request: web::Data<mpsc::Sender<CommandRequest>>) -> HttpResponse {
        debug!("WebService status");

        let pubs = PUBLISH_CHANNEL_MAP.lock().await;

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
            error!("execute request: command receiver droped");
            return HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR).finish();
        }

        let Ok(result) = rx_reply.await else {
            error!("execute request: command sender droped");
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

pub async fn publish(channel: PublishChannel, value: serde_json::Value) {
    debug!("publish");
    if !(*IS_WEBSERVICE_ENABLED) {
        debug!("publish: skip since feature not enabled");
    } else {
        let msg = json!({"channel": channel.to_string(), "data": value});

        PUBLISH_CHANNEL_MAP
            .lock()
            .await
            .insert(channel.to_string(), value.clone());

        if let Err(e) = publish_to_endpoints(msg).await {
            warn!("publish: failed to publish request with {e:#}");
        };
    }
}

async fn publish_to_endpoints(msg: serde_json::Value) -> Result<()> {
    debug!("publish to endpoints");

    let endpoints: Vec<PublishEndpointRequest> =
        match std::fs::read_to_string(centrifugo_endpoints_path!()) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| Vec::new()),
            Err(_) => Vec::new(),
        };

    let mut errors = Vec::new();

    for endpoint in endpoints {
        let mut headers = header::HeaderMap::new();

        for h in &endpoint.endpoint.headers {
            headers.insert(
                header::HeaderName::from_str(&h.name)?,
                header::HeaderValue::from_str(&h.value)?,
            );
        }

        // we allow self-signed ssl certs
        let Ok(request) = Client::builder()
            .timeout(tokio::time::Duration::from_secs(3))
            .default_headers(headers)
            .danger_accept_invalid_certs(true)
            .build()
        else {
            // bail!(
            //     "publish_to_endpoints: building request {msg} to {} failed",
            //     endpoint.endpoint.url
            // );
            errors.push(format!(
                "publish_to_endpoints: building request {msg} to {} failed",
                endpoint.endpoint.url
            ));
            continue;
        };
        if let Err(e) = request
            .post(&endpoint.endpoint.url)
            .body(msg.to_string())
            .send()
            .await
        {
            //     bail!(
            //     "publish_to_endpoints: sending {msg} to {} (failed with {e}). Endpoint not present?",
            //     endpoint.endpoint.url
            // );
            errors.push(format!("publish_to_endpoints: sending {msg} to {} (failed with {e}). Endpoint not present?",
            endpoint.endpoint.url));
            continue;
        }

        info!(
            "publish_to_endpoints: successfully sent {msg} to {}.",
            endpoint.endpoint.url
        )
    }
    if !errors.is_empty() {
        // Return the first error, or combine them as needed
        //return Err(anyhow::anyhow!("Errors occurred: {:?}", errors));
        bail!("Errors occurred: {:?}", errors)
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use actix_web::{test, App};

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
