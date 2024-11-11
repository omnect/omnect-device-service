use crate::twin::feature::*;
use actix_server::ServerHandle;
use actix_web::{http::StatusCode, web, App, HttpResponse, HttpServer};
use anyhow::{Context, Result};
use futures_util::StreamExt as _;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use reqwest::{header, Client, RequestBuilder};
use serde::Deserialize;
use serde_json::json;
use std::{str::FromStr, sync::OnceLock};
use tokio::sync::{mpsc, oneshot, Mutex};

static PUBLISH_CHANNEL_MAP: OnceLock<Mutex<serde_json::Map<String, serde_json::Value>>> =
    OnceLock::new();
static PUBLISH_ENDPOINTS: OnceLock<Mutex<Vec<PublishEndpoint>>> = OnceLock::new();

#[derive(Debug)]
pub struct Request {
    pub(crate) command: Command,
    pub(crate) reply: oneshot::Sender<CommandResult>,
}

#[derive(Debug, strum_macros::Display)]
pub enum PublishChannel {
    FactoryResetKeys,
    FactoryResetStatus,
    NetworkStatus,
    OnlineStatus,
    SystemInfo,
    Timeouts,
}

#[derive(Deserialize, Debug)]
struct Header {
    name: String,
    value: String,
}

#[derive(Deserialize, Debug)]
struct PublishEndpoint {
    url: String,
    headers: Vec<Header>,
}

lazy_static! {
    static ref WEBSERVICE_ENABLED: bool = {
        std::env::var("WEBSERVICE_ENABLED")
            .unwrap_or("false".to_string())
            .to_lowercase()
            == "true"
    };
}

macro_rules! publish_endpoints_path {
    () => {{
        static PUBLISH_ENDPOINTS_PATH_DEFAULT: &'static str = "/etc/omnect/publish_endpoints.json";
        std::env::var("PUBLISH_ENDPOINTS_PATH")
            .unwrap_or(PUBLISH_ENDPOINTS_PATH_DEFAULT.to_string())
    }};
}

pub struct WebService {
    srv_handle: ServerHandle,
}

impl WebService {
    pub async fn run(tx_request: mpsc::Sender<Request>) -> Result<Option<Self>> {
        // we only start web service feature if WEBSERVICE_ENABLED env var is explicitly set
        if !(*WEBSERVICE_ENABLED) {
            info!("WebService is disabled");
            return Ok(None);
        };

        info!("WebService is enabled");

        let _unused = PUBLISH_CHANNEL_MAP
            .get_or_init(|| Mutex::new(serde_json::Map::default()))
            .lock()
            .await;

        let _unused = PUBLISH_ENDPOINTS
            .get_or_init(|| {
                let Ok(true) = std::path::Path::new(&publish_endpoints_path!()).try_exists() else {
                    info!("run: no endpoint file present");

                    return Mutex::new(vec![]);
                };

                Mutex::new(
                    serde_json::from_reader(std::io::BufReader::new(
                        std::fs::File::open(publish_endpoints_path!())
                            .expect("cannot open endpoints file"),
                    ))
                    .expect("cannot parse endpoints file"),
                )
            })
            .lock()
            .await;

        let srv = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(tx_request.clone()))
                .route("/factory-reset/v1", web::post().to(Self::factory_reset))
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

    async fn factory_reset(
        mut body: web::Payload,
        tx_request: web::Data<mpsc::Sender<Request>>,
    ) -> HttpResponse {
        debug!("WebService factory_reset");

        let mut bytes = web::BytesMut::new();
        while let Some(item) = body.next().await {
            let Ok(item) = item else {
                error!("couldn't read body from stream");
                return HttpResponse::build(StatusCode::BAD_REQUEST)
                    .body("couldn't read body from stream");
            };

            bytes.extend_from_slice(&item);
        }

        match serde_json::from_slice(&bytes) {
            Ok(command) => {
                let (tx_reply, rx_reply) = oneshot::channel();
                let req = Request {
                    command: Command::FactoryReset(command),
                    reply: tx_reply,
                };

                Self::exec_request(tx_request.as_ref(), rx_reply, req).await
            }
            Err(e) => {
                error!("couldn't parse FactoryResetCommand from body: {e}");
                HttpResponse::build(StatusCode::BAD_REQUEST).body(e.to_string())
            }
        }
    }

    async fn reboot(tx_request: web::Data<mpsc::Sender<Request>>) -> HttpResponse {
        debug!("WebService reboot");

        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = Request {
            command: Command::Reboot,
            reply: tx_reply,
        };

        Self::exec_request(tx_request.as_ref(), rx_reply, cmd).await
    }

    async fn reload_network(tx_request: web::Data<mpsc::Sender<Request>>) -> HttpResponse {
        debug!("WebService reload_network");

        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = Request {
            command: Command::ReloadNetwork,
            reply: tx_reply,
        };

        Self::exec_request(tx_request.as_ref(), rx_reply, cmd).await
    }

    async fn republish(_tx_request: web::Data<mpsc::Sender<Request>>) -> HttpResponse {
        debug!("WebService republish");

        for (channel, value) in PUBLISH_CHANNEL_MAP
            .get()
            .expect("PUBLISH_CHANNEL_MAP not available")
            .lock()
            .await
            .iter()
        {
            let msg = json!({"channel": channel, "data": value});

            for endpoint in PUBLISH_ENDPOINTS
                .get()
                .expect("PUBLISH_ENDPOINTS not available")
                .lock()
                .await
                .iter()
            {
                let Ok(reqest) = publish_request(&msg, endpoint) else {
                    error!(
                        "republish: building request {msg} to {} failed",
                        endpoint.url
                    );
                    return HttpResponse::InternalServerError().finish();
                };

                if let Err(e) = reqest.send().await {
                    warn!(
                        "republish: sending request {msg} to {} failed with {e}. Endpoint not present?",
                        endpoint.url);
                    return HttpResponse::InternalServerError().finish();
                }
            }
        }

        HttpResponse::Ok().finish()
    }

    async fn status(_tx_request: web::Data<mpsc::Sender<Request>>) -> HttpResponse {
        debug!("WebService status");

        let pubs = PUBLISH_CHANNEL_MAP
            .get()
            .expect("PUBLISH_CHANNEL_MAP not available")
            .lock()
            .await;

        let pubs = pubs.to_owned();

        let pubs = serde_json::to_string(&pubs).expect("cannot convert publish map to string");

        HttpResponse::Ok().body(pubs)
    }

    async fn exec_request(
        tx_request: &mpsc::Sender<Request>,
        rx_reply: tokio::sync::oneshot::Receiver<CommandResult>,
        request: Request,
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
                error!("execute request: request failed {e}");
                HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR).body(e.to_string())
            }
        }
    }
}

pub async fn publish(channel: PublishChannel, value: serde_json::Value) -> Result<()> {
    if !(*WEBSERVICE_ENABLED) {
        debug!("publish: skip since feature not enabled");
        return Ok(());
    }

    let msg = json!({"channel": channel.to_string(), "data": value});

    PUBLISH_CHANNEL_MAP
        .get()
        .expect("PUBLISH_CHANNEL_MAP not available")
        .lock()
        .await
        .insert(channel.to_string(), value.clone());

    for endpoint in PUBLISH_ENDPOINTS
        .get()
        .expect("PUBLISH_ENDPOINTS not available")
        .lock()
        .await
        .iter()
    {
        let Ok(request) = publish_request(&msg, endpoint) else {
            error!("publish: building request {msg} to {} failed", endpoint.url);
            continue;
        };

        if let Err(e) = request.send().await {
            warn!(
                "publish: sending request {msg} to {} failed with {e}. Endpoint not present?",
                endpoint.url
            );
        }
    }

    Ok(())
}

fn publish_request(msg: &serde_json::Value, endpoint: &PublishEndpoint) -> Result<RequestBuilder> {
    let mut headers = header::HeaderMap::new();

    debug!("publish_request {msg} to {}", endpoint.url);

    for h in &endpoint.headers {
        headers.insert(
            header::HeaderName::from_str(&h.name)?,
            header::HeaderValue::from_str(&h.value)?,
        );
    }

    // we allow self-signed ssl certs
    Ok(Client::builder()
        .default_headers(headers)
        .danger_accept_invalid_certs(true)
        .build()?
        .post(&endpoint.url)
        .body(msg.to_string()))
}

#[cfg(test)]
mod tests {
    use actix_web::{test, App};

    use super::*;

    #[actix_web::test]
    async fn reboot_ok() {
        let (tx_web_service, mut rx_web_service) = tokio::sync::mpsc::channel::<Request>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/reboot/v1", web::post().to(WebService::reboot)),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply.send(Ok(None)).unwrap();
        });

        let req = test::TestRequest::post().uri("/reboot/v1").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn reboot_fail() {
        let (tx_web_service, mut rx_web_service) = tokio::sync::mpsc::channel::<Request>(100);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(tx_web_service.clone()))
                .route("/reboot/v1", web::post().to(WebService::reboot)),
        )
        .await;

        tokio::spawn(async move {
            let req = rx_web_service.recv().await.unwrap();
            req.reply.send(Err(anyhow::anyhow!("error"))).unwrap();
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
