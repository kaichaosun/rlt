/// Start a localtunnel server,
/// request a proxy endpoint at `domain.tld/<your-endpoint>`,
/// user's request then proxied via `<your-endpoint>.domain.tld`.
use std::{sync::Arc, net::SocketAddr};

use actix_web::{web, App, HttpServer};
use hyper::{service::service_fn, server::conn::http1, header::{UPGRADE, HOST}, upgrade::OnUpgrade, StatusCode};
use tokio::{net::TcpListener, sync::Mutex};

use crate::api::{api_status, request_endpoint};
use crate::state::{State, ClientManager};
use crate::proxy::proxy_handler;

mod api;
mod state;
mod proxy;

/// Start the proxy use low level api from hyper.
/// Proxy endpoint request is served via actix-web.
// TODO: add require_auth: bool
pub async fn create(domain: String, api_port: u16, secure: bool, max_sockets: u8, proxy_port: u16) {
    log::info!("Api server listens at {} {}", &domain, api_port);
    log::info!("Start proxy server at {} {}, options: {} {}", &domain, proxy_port, secure,  max_sockets);

    let manager = Arc::new(Mutex::new(ClientManager::new()));
    let api_state = web::Data::new(State {
        manager: manager.clone(),
    });

    tokio::spawn(async move {
        let addr: SocketAddr = ([127, 0, 0, 1], proxy_port).into();
        let listener = TcpListener::bind(addr).await.unwrap();

        loop {
            let (stream, _) = listener.accept().await.unwrap();
            log::info!("Accepted a new proxy request");

            // This is the `Service` that will handle the connection.
            // `service_fn` is a helper to convert a function that
            // returns a Response into a `Serive`.
            // TODO extract to function
            let proxy_manager = manager.clone();
            let service = service_fn(move |mut req| {
                proxy_handler(req, proxy_manager.clone())
            });

            tokio::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(stream, service)
                    .with_upgrades()
                    .await
                {
                    log::error!("Failed to serve connection: {:?}", err);
                }
            });
        }
    });

    HttpServer::new(move || {
        App::new()
            .app_data(api_state.clone())
            .service(api_status)
            .service(request_endpoint)
    })
    .bind(("127.0.0.1", api_port)).unwrap()
    .run()
    .await
    .unwrap();
}

fn extract(hostname: String) -> String {
    // TODO regex
    let hostname = hostname
        .replace("http://", "")
        .replace("https://", "")
        .replace("ws", "")
        .replace("wss", "");

    hostname.split(".").next().unwrap().to_string()
}
