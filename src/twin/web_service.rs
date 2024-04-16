use actix_server::ServerHandle;
use actix_web::{web, App, HttpServer, Responder};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum Command {
    Reboot(Reply),
    GetOsVersion(Reply),
}

type Reply = oneshot::Sender<serde_json::Value>;

pub struct WebService {
    srv_handle: ServerHandle,
}

impl WebService {
    pub fn new(tx_request: mpsc::Sender<Command>) -> Self {
        let srv = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(tx_request.clone()))
                .route("/reboot", web::put().to(Self::reboot))
                .route("/os-version", web::get().to(Self::os_version))
        })
        .bind("localhost:12347")
        .unwrap()
        .run();
        let srv_handle = srv.handle();

        tokio::spawn(srv);

        /*     let response = reqwest::get("http://localhost:12347").await.unwrap();
        println!("Response code: {:?}", response.status()); */
        WebService {
            srv_handle,
        }
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

        rx_reply.await.unwrap().to_string()
    }
}
