/// Start a localtunnel server,
/// request a proxy endpoint at `domain.tld/<your-endpoint>`,
/// user's request then proxied via `<your-endpoint>.domain.tld`.

#[macro_use]
extern crate lazy_static;

use std::{sync::Arc, net::SocketAddr, io};

use actix_web::{web, App, HttpServer};
use hyper::{service::service_fn, server::conn::http1};
use tokio::{net::TcpListener, sync::Mutex};
use dotenv::dotenv;

use crate::api::{api_status, request_endpoint};
use crate::config::Config;
use crate::state::{State, ClientManager};
use crate::proxy::proxy_handler;

mod api;
mod state;
mod proxy;
mod auth;
mod config;

lazy_static! {
    static ref CONFIG: Config = {
        dotenv().ok();
        envy::from_env::<Config>().unwrap()
    };
}

/// Start the proxy use low level api from hyper.
/// Proxy endpoint request is served via actix-web.
pub async fn start(domain: String, api_port: u16, secure: bool, max_sockets: u8, proxy_port: u16, require_auth: bool) -> io::Result<()> {
    log::info!("Api server listens at {} {}", &domain, api_port);
    log::info!("Start proxy server at {} {}, options: {} {}", &domain, proxy_port, secure,  max_sockets);

    let manager = Arc::new(Mutex::new(ClientManager::new()));
    let api_state = web::Data::new(State {
        manager: manager.clone(),
        max_sockets,
        require_auth,
    });

    let proxy_addr: SocketAddr = ([127, 0, 0, 1], proxy_port).into();
    let listener = TcpListener::bind(proxy_addr).await?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    log::info!("Accepted a new proxy request");

                    let proxy_manager = manager.clone();
                    let service = service_fn(move |req| {
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
                },
                Err(e) => log::error!("Failed to accept the request: {:?}", e),
            }
        }
    });

    HttpServer::new(move || {
        App::new()
            .app_data(api_state.clone())
            .service(api_status)
            .service(request_endpoint)
    })
    .bind(("127.0.0.1", api_port))?
    .run()
    .await?;

    Ok(())
}
