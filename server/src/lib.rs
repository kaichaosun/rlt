//! Start a localtunnel server,
//! request a proxy endpoint at `domain.tld/<your-endpoint>`,
//! user's request then proxied via `<your-endpoint>.domain.tld`.

#[macro_use]
extern crate lazy_static;

use std::{net::SocketAddr, sync::Arc};

use actix_web::{web, App, HttpServer};
use anyhow::Result;
use dotenv::dotenv;
use hyper::{server::conn::http1, service::service_fn};
use tokio::{net::TcpListener, sync::Mutex};

use crate::api::{api_status, request_endpoint};
use crate::config::Config;
use crate::proxy::proxy_handler;
use crate::state::{ClientManager, State};

mod api;
mod auth;
mod config;
mod error;
mod proxy;
mod state;

lazy_static! {
    static ref CONFIG: Config = {
        dotenv().ok();
        envy::from_env::<Config>().unwrap_or_default()
    };
}

pub struct ServerConfig {
    pub domain: String,
    pub api_port: u16,
    pub secure: bool,
    pub max_sockets: u8,
    pub proxy_port: u16,
    pub require_auth: bool,
}

/// Start the proxy use low level api from hyper.
/// Proxy endpoint request is served via actix-web.
pub async fn start(config: ServerConfig) -> Result<()> {
    let ServerConfig {
        domain,
        api_port,
        secure,
        max_sockets,
        proxy_port,
        require_auth,
    } = config;
    log::info!("Api server listens at {} {}", &domain, api_port);
    log::info!(
        "Start proxy server at {} {}, options: {} {}, require auth: {}",
        &domain,
        proxy_port,
        secure,
        max_sockets,
        require_auth
    );

    let manager = Arc::new(Mutex::new(ClientManager::new(max_sockets)));
    let api_state = web::Data::new(State {
        manager: manager.clone(),
        max_sockets,
        require_auth,
        secure,
        domain,
    });

    let proxy_addr: SocketAddr = ([0, 0, 0, 0], proxy_port).into();
    let listener = TcpListener::bind(proxy_addr).await?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    log::info!("Accepted a new proxy request");

                    let proxy_manager = manager.clone();
                    let service = service_fn(move |req| proxy_handler(req, proxy_manager.clone()));

                    tokio::spawn(async move {
                        if let Err(err) = http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                            .with_upgrades()
                            .await
                        {
                            log::error!("Failed to serve connection: {:?}", err);
                        }
                    });
                }
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
    .bind(("0.0.0.0", api_port))?
    .run()
    .await?;

    Ok(())
}
