use actix_server::ServerHandle;
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum Command {
    Reboot(Reply),
    GetOsVersion(Reply),
    ReloadNetwork(Reply),
}

type Reply = oneshot::Sender<serde_json::Value>;

pub struct WebService {
    srv_handle: ServerHandle,
}

impl WebService {
    #[cfg(not(feature = "mock"))]
    pub fn new(tx_request: mpsc::Sender<Command>) -> Result<Self> {
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

        listener
            .set_nonblocking(true)
            .context("web_service: cannot create UnixListener")?;

        let srv = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(tx_request.clone()))
                .route("/reboot", web::put().to(Self::reboot))
                .route("/os-version", web::get().to(Self::os_version))
                .route("/reload-network", web::put().to(Self::reload_network))
        })
        .listen_uds(listener)
        .context("web_service: cannot listen on UnixListener")?
        .run();

        let srv_handle = srv.handle();

        tokio::spawn(srv);

        Ok(WebService { srv_handle })
    }

    #[cfg(feature = "mock")]
    pub fn new(tx_request: mpsc::Sender<Command>) -> Result<Self> {
        const SOCKET_PATH: &str = "/tmp/api.sock";
        let srv = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(tx_request.clone()))
                .route("/reboot", web::put().to(Self::reboot))
                .route("/os-version", web::get().to(Self::os_version))
                .route("/reload-network", web::put().to(Self::reload_network))
        })
        .bind_uds(SOCKET_PATH)
        .context(format!("web_service: cannot bind to {SOCKET_PATH}"))?
        .run();

        let srv_handle = srv.handle();

        tokio::spawn(srv);

        Ok(WebService { srv_handle })
    }

    pub async fn shutdown(&self) {
        self.srv_handle.stop(false).await;
    }

    async fn reboot(tx_request: web::Data<mpsc::Sender<Command>>) -> impl Responder {
        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = Command::Reboot(tx_reply);

        tx_request.send(cmd).await.unwrap();

        rx_reply.await.unwrap().to_string()
    }

    async fn os_version(tx_request: web::Data<mpsc::Sender<Command>>) -> impl Responder {
        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = Command::GetOsVersion(tx_reply);

        tx_request.send(cmd).await.unwrap();

        HttpResponse::Ok()
            .content_type("application/json")
            .body(rx_reply.await.unwrap().to_string())
    }

    async fn reload_network(tx_request: web::Data<mpsc::Sender<Command>>) -> impl Responder {
        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = Command::ReloadNetwork(tx_reply);

        tx_request.send(cmd).await.unwrap();

        rx_reply.await.unwrap().to_string()
    }
}
