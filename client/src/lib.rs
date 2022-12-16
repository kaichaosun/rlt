use std::sync::Arc;
use tokio::sync::Semaphore;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Duration, sleep};

pub use tokio::sync::broadcast;

pub const PROXY_SERVER: &str = "https://localtunnel.me";
pub const LOCAL_HOST: &str = "127.0.0.1";

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

/// Open tunnels directly between server and localhost
pub async fn open_tunnel(
    server: Option<&str>,
    subdomain: Option<&str>,
    local_host: Option<&str>,
    local_port: u16,
    shutdown_signal: broadcast::Sender<()>,
    max_conn: u8,
    credential: Option<String>,
) -> Result<String> {
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
    server: Option<&str>,
    subdomain: Option<&str>,
    credential: Option<String>,
) -> Result<TunnelServerInfo> {
    let server = server.unwrap_or(PROXY_SERVER);
    let assigned_domain = subdomain.unwrap_or("?new");
    let mut uri = format!("{}/{}", server, assigned_domain);
    if let Some(credential) = credential {
        uri = format!("{}?credential={}", uri, credential);
    }
    log::info!("Request for assign domain: {}", uri);

    let resp = reqwest::get(uri).await?.json::<ProxyResponse>().await?;
    log::info!("Response from server: {:#?}", resp);

    let parts = resp.url.split("//").collect::<Vec<&str>>();
    let mut host = parts[1].split(":").collect::<Vec<&str>>()[0];
    host = match host.split_once(".") {
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
    local_host: Option<&str>,
    local_port: u16,
    shutdown_signal: broadcast::Sender<()>,
    max_conn: u8,
) {
    log::info!("Tunnel server info: {:?}", server);
    let server_host = server.host;
    let server_port = server.port;
    let local_host = local_host.unwrap_or(LOCAL_HOST).to_string();

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
                                        log::error!("Failed to connect to proxy server: {:?}", err);
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
    let remote_stream = TcpStream::connect(format!("{}:{}", remote_host, remote_port)).await?;
    let local_stream = TcpStream::connect(format!("{}:{}", local_host, local_port)).await?;

    proxy(remote_stream, local_stream).await?;
    Ok(())
}

/// Copy data mutually between two read/write streams.
async fn proxy<S1, S2>(stream1: S1, stream2: S2) -> io::Result<()>
where
    S1: AsyncRead + AsyncWrite + Unpin,
    S2: AsyncRead + AsyncWrite + Unpin,
{
    let (mut s1_read, mut s1_write) = io::split(stream1);
    let (mut s2_read, mut s2_write) = io::split(stream2);
    tokio::select! {
        res = io::copy(&mut s1_read, &mut s2_write) => res,
        res = io::copy(&mut s2_read, &mut s1_write) => res,
    }?;

    Ok(())
}
