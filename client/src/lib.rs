use std::sync::Arc;
use tokio::sync::Semaphore;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use tokio::io;
use tokio::net::TcpStream;
pub use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

pub const PROXY_SERVER: &str = "https://init.so";
pub const LOCAL_HOST: &str = "127.0.0.1";

// See https://tldp.org/HOWTO/html_single/TCP-Keepalive-HOWTO to understand how keepalive work.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const TCP_KEEPALIVE_RETRIES: u32 = 5;

#[derive(Debug, Serialize, Deserialize)]
struct ProxyResponse {
    id: String,
    port: u16,
    max_conn_count: u8,
    url: String,
}

/// The server detail for client to connect
#[derive(Clone, Debug)]
pub struct TunnelServerInfo {
    pub host: String,
    pub port: u16,
    pub max_conn_count: u8,
    pub url: String,
}

pub struct ClientConfig {
    pub server: Option<String>,
    pub subdomain: Option<String>,
    pub local_host: Option<String>,
    pub local_port: u16,
    pub shutdown_signal: broadcast::Sender<()>,
    pub max_conn: u8,
    pub credential: Option<String>,
}

/// Open tunnels directly between server and localhost
pub async fn open_tunnel(config: ClientConfig) -> Result<String> {
    let ClientConfig {
        server,
        subdomain,
        local_host,
        local_port,
        shutdown_signal,
        max_conn,
        credential,
    } = config;
    let tunnel_info = get_tunnel_endpoint(server, subdomain, credential).await?;

    // TODO check the connect is failed and restart the proxy.
    tunnel_to_endpoint(
        tunnel_info.clone(),
        local_host,
        local_port,
        shutdown_signal,
        max_conn,
    )
    .await;

    Ok(tunnel_info.url)
}

async fn get_tunnel_endpoint(
    server: Option<String>,
    subdomain: Option<String>,
    credential: Option<String>,
) -> Result<TunnelServerInfo> {
    let server = server.as_deref().unwrap_or(PROXY_SERVER);
    let assigned_domain = subdomain.as_deref().unwrap_or("?new");
    let mut uri = format!("{}/{}", server, assigned_domain);
    if let Some(credential) = credential {
        uri = format!("{}?credential={}", uri, credential);
    }
    log::info!("Request for assign domain: {}", uri);

    let resp = reqwest::get(uri).await?.json::<ProxyResponse>().await?;
    log::info!("Response from server: {:#?}", resp);

    let parts = resp.url.split("//").collect::<Vec<&str>>();
    let mut host = parts[1].split(':').collect::<Vec<&str>>()[0];
    host = match host.split_once('.') {
        Some((_, base)) => base,
        None => host,
    };

    let tunnel_info = TunnelServerInfo {
        host: host.to_string(),
        port: resp.port,
        max_conn_count: resp.max_conn_count,
        url: resp.url,
    };

    Ok(tunnel_info)
}

async fn tunnel_to_endpoint(
    server: TunnelServerInfo,
    local_host: Option<String>,
    local_port: u16,
    shutdown_signal: broadcast::Sender<()>,
    max_conn: u8,
) {
    log::info!("Tunnel server info: {:?}", server);
    let server_host = server.host;
    let server_port = server.port;
    let local_host = local_host.unwrap_or(LOCAL_HOST.to_string());

    let count = std::cmp::min(server.max_conn_count, max_conn);
    log::info!("Max connection count: {}", count);
    let limit_connection = Arc::new(Semaphore::new(count.into()));

    let mut shutdown_receiver = shutdown_signal.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = limit_connection.clone().acquire_owned() => {
                    let permit = match res {
                        Ok(permit) => permit,
                        Err(err) => {
                            log::error!("Acquire limit connection failed: {:?}", err);
                            return;
                        },
                    };
                    let server_host = server_host.clone();
                    let local_host = local_host.clone();

                    let mut shutdown_receiver = shutdown_signal.subscribe();

                    tokio::spawn(async move {
                        log::info!("Create a new proxy connection.");
                        tokio::select! {
                            res = handle_connection(server_host, server_port, local_host, local_port) => {
                                match res {
                                    Ok(_) => log::info!("Connection result: {:?}", res),
                                    Err(err) => {
                                        log::error!("Failed to connect to proxy or local server: {:?}", err);
                                        sleep(Duration::from_secs(10)).await;
                                    }
                                }
                            }
                            _ = shutdown_receiver.recv() => {
                                log::info!("Shutting down the connection immediately");
                            }
                        }

                        drop(permit);
                    });
                }
                _ = shutdown_receiver.recv() => {
                    log::info!("Shuttign down the loop immediately");
                    return;
                }
            };
        }
    });
}

async fn handle_connection(
    remote_host: String,
    remote_port: u16,
    local_host: String,
    local_port: u16,
) -> Result<()> {
    log::debug!("Connect to remote: {}, {}", remote_host, remote_port);
    let mut remote_stream = TcpStream::connect(format!("{}:{}", remote_host, remote_port)).await?;
    log::debug!("Connect to local: {}, {}", local_host, local_port);
    let mut local_stream = TcpStream::connect(format!("{}:{}", local_host, local_port)).await?;

    // configure keepalive on remote socket to early detect network issues and attempt to re-establish the connection.
    let ka = TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_TIME)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);
    let sf = SockRef::from(&remote_stream);
    sf.set_tcp_keepalive(&ka)?;

    io::copy_bidirectional(&mut remote_stream, &mut local_stream).await?;
    Ok(())
}
