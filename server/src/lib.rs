/// Start a localtunnel server,
/// request a proxy endpoint at `domain.tld/<your-endpoint>`,
/// user's request then proxied via `<your-endpoint>.domain.tld`.
use std::{sync::{Mutex, Arc}, net::SocketAddr};

use actix_web::{web, App, HttpServer};
use hyper::{service::service_fn, server::conn::http1, header::{UPGRADE, HOST}, upgrade::OnUpgrade, StatusCode};
use tokio::net::TcpListener;

use crate::api::{api_status, request_endpoint};
use crate::state::{State, ClientManager};

mod api;
mod state;

/// Start the proxy use low level api from hyper.
/// Proxy endpoint request is served via actix-web.
// TODO: add require_auth: bool
pub async fn create(domain: String, api_port: u16, secure: bool, max_sockets: u8, proxy_port: u16) {
    log::info!("Listening api server at {} {}", &domain, api_port);
    log::info!("Create proxy server at {} {}, options: {} {}", &domain, proxy_port, secure,  max_sockets);

    let manager = Arc::new(Mutex::new(ClientManager::new()));
    let state = web::Data::new(State {
        manager: manager.clone(),
    });

    tokio::spawn(async move {
        let addr: SocketAddr = ([127, 0, 0, 1], proxy_port).into();
        log::info!("listening on {}", addr);
        let listener = TcpListener::bind(addr).await.unwrap();

        loop {
            let (stream, _) = listener.accept().await.unwrap();

            log::info!("Accept proxy request");

            
            let manager = manager.clone();

            // This is the `Service` that will handle the connection.
            // `service_fn` is a helper to convert a function that
            // returns a Response into a `Serive`.
            // TODO extract to function
            let service = service_fn(move |mut req| {
                log::info!("uri ========= {}", req.uri());
                log::info!("host ========= {:?}", req.headers());
                let hostname = req.headers().get(HOST).unwrap().to_str().unwrap();
                log::info!("hostname ========= {}", hostname);

                let endpoint = extract(hostname.to_string());
                let mut manager = manager.lock().unwrap();
                log::info!("endpoint: {}", endpoint);
                let client = manager.clients.get_mut(&endpoint).unwrap();
                let mut client = client.lock().unwrap();
                let client_stream = client.take().unwrap();

                async move {
                    if !req.headers().contains_key(UPGRADE) {
                        let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await.unwrap();
                        tokio::spawn(async move {
                            if let Err(err) = conn.await {
                                log::error!("Connection failed: {:?}", err);
                            }
                        });
    
                        sender.send_request(req).await
                    } else {
                        let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await.unwrap();
                            tokio::spawn(async move {
                                if let Err(err) = conn.await {
                                    log::error!("Connection failed: {:?}", err);
                                }
                            });
    
                        let request_upgrade_type = req.headers().get(UPGRADE).unwrap().to_str().unwrap().to_string();
                        let request_upgraded = req.extensions_mut().remove::<OnUpgrade>().unwrap();
    
                        let mut response = sender.send_request(req).await.unwrap();
    
                        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                            let response_upgrade_type = response.headers().get(UPGRADE).unwrap().to_str().unwrap().to_string();
                            if request_upgrade_type == response_upgrade_type {
                                let mut response_upgraded = response.extensions_mut().remove::<OnUpgrade>()
                                    .expect("response does not have an upgrade extension")
                                    .await.unwrap();
    
                                log::info!("Responding to a connection upgrade response");
    
                                tokio::spawn(async move {
                                    let mut request_upgraded = request_upgraded.await.expect("failed to upgrade request");
                                    tokio::io::copy_bidirectional(&mut response_upgraded, &mut request_upgraded)
                                        .await
                                        .expect("coping between upgraded connections failed");
                                });
                            }
                            Ok(response)
                        } else {
                            Ok(response)
                        }
                    }
                }
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
            .app_data(state.clone())
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
