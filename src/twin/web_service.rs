use actix_server::ServerHandle;
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum Command {
    Reboot(Reply),
    GetOsVersion(Reply),
    RestartNetwork(Reply),
}

type Reply = oneshot::Sender<serde_json::Value>;

pub struct WebService {
    srv_handle: ServerHandle,
}

impl WebService {
    pub fn new(tx_request: mpsc::Sender<Command>) -> Self {
        // @ToDo bind to unix domain socket
        let srv = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(tx_request.clone()))
                .route("/reboot", web::put().to(Self::reboot))
                .route("/os-version", web::get().to(Self::os_version))
                .route("/restart-network", web::put().to(Self::restart_network))
        })
        .bind("localhost:1977")
        .unwrap()
        .run();
        let srv_handle = srv.handle();

        tokio::spawn(srv);

        WebService { srv_handle }
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

    async fn restart_network(tx_request: web::Data<mpsc::Sender<Command>>) -> impl Responder {
        let (tx_reply, rx_reply) = oneshot::channel();
        let cmd = Command::RestartNetwork(tx_reply);

        tx_request.send(cmd).await.unwrap();

        rx_reply.await.unwrap().to_string()
    }
}
