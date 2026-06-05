use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use tokio::io;
use tokio::net::TcpStream;
pub use tokio::sync::broadcast;
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, Duration};

pub const PROXY_SERVER: &str = "https://your-domain.com";
pub const LOCAL_HOST: &str = "127.0.0.1";

// See https://tldp.org/HOWTO/html_single/TCP-Keepalive-HOWTO to understand how keepalive work.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(not(target_os = "windows"))]
const TCP_KEEPALIVE_RETRIES: u32 = 5;

/// Default for [`ClientConfig::reregister_after`]: how long the remote endpoint
/// must be *continuously* unreachable before we re-register. Time-based rather
/// than failure-count based, so the trigger is independent of how many
/// connections happen to be open — a brief blip (a quick server restart, a
/// momentary network hiccup) that keepalive + reconnect can ride out on its own
/// won't force a costly re-registration.
pub const DEFAULT_REREGISTER_AFTER: Duration = Duration::from_secs(30);

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
    /// How long the remote endpoint must be continuously unreachable before the
    /// tunnel re-registers. `None` uses [`DEFAULT_REREGISTER_AFTER`].
    pub reregister_after: Option<Duration>,
}

/// Open tunnels directly between server and localhost.
///
/// Registers a tunnel endpoint, then spawns a supervisor that maintains
/// connections and automatically re-registers if the endpoint becomes
/// unreachable.
pub async fn open_tunnel(config: ClientConfig) -> Result<String> {
    let ClientConfig {
        server,
        subdomain,
        local_host,
        local_port,
        shutdown_signal,
        max_conn,
        credential,
        reregister_after,
    } = config;
    let tunnel_info =
        get_tunnel_endpoint(server.clone(), subdomain.clone(), credential.clone()).await?;
    let url = tunnel_info.url.clone();

    let supervisor_config = SupervisorConfig {
        server,
        subdomain,
        credential,
        local_host,
        local_port,
        shutdown_signal,
        max_conn,
        reregister_after: reregister_after.unwrap_or(DEFAULT_REREGISTER_AFTER),
    };
    tokio::spawn(tunnel_supervisor(supervisor_config, tunnel_info));

    Ok(url)
}

struct SupervisorConfig {
    server: Option<String>,
    subdomain: Option<String>,
    credential: Option<String>,
    local_host: Option<String>,
    local_port: u16,
    shutdown_signal: broadcast::Sender<()>,
    max_conn: u8,
    reregister_after: Duration,
}

// Runs the register → connect → detect-failures → re-register cycle.
//
// Each iteration ("round") spawns a pool of connections to the current endpoint.
// If the remote endpoint stays unreachable for `reregister_after` (the server
// cleaned up our listener port, or the network path changed), connection tasks
// signal via `reregister_tx` and the supervisor requests a fresh endpoint from
// the API server—using the same subdomain so the public tunnel URL stays stable.
async fn tunnel_supervisor(config: SupervisorConfig, initial_info: TunnelServerInfo) {
    let mut current_info = initial_info;
    let mut shutdown_rx = config.shutdown_signal.subscribe();
    let reregister_after = config.reregister_after;

    loop {
        log::info!("Starting tunnel connections to {:?}", current_info);

        // Per-round shutdown channel: lets us stop this round's connections
        // without tearing down the whole supervisor.
        let (round_stop_tx, _) = broadcast::channel::<()>(1);
        let (reregister_tx, mut reregister_rx) = mpsc::channel::<()>(1);
        let health = RoundHealth::new(reregister_after, reregister_tx);

        start_tunnel_connections(
            &current_info,
            config.local_host.clone(),
            config.local_port,
            round_stop_tx.clone(),
            config.max_conn,
            health,
        );

        // Block until either the connections ask for re-registration or we
        // are told to shut down entirely.
        tokio::select! {
            _ = reregister_rx.recv() => {
                log::warn!(
                    "Re-registering tunnel after endpoint unreachable for {:?}",
                    reregister_after
                );
                let _ = round_stop_tx.send(());
                sleep(Duration::from_millis(500)).await;
            }
            _ = shutdown_rx.recv() => {
                let _ = round_stop_tx.send(());
                return;
            }
        }

        // Re-register with exponential backoff (2 s → 4 s → … → 60 s cap).
        // The same subdomain is requested so the public URL doesn't change;
        // only the internal listener port is refreshed.
        let mut backoff = Duration::from_secs(2);
        loop {
            match get_tunnel_endpoint(
                config.server.clone(),
                config.subdomain.clone(),
                config.credential.clone(),
            )
            .await
            {
                Ok(info) => {
                    log::info!("Re-registered tunnel endpoint: {:?}", info);
                    current_info = info;
                    break;
                }
                Err(err) => {
                    log::error!(
                        "Re-registration failed: {:?}, retrying in {:?}",
                        err,
                        backoff
                    );
                    tokio::select! {
                        _ = sleep(backoff) => {
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                        }
                        _ = shutdown_rx.recv() => return,
                    }
                }
            }
        }
    }
}

/// Tracks remote-endpoint health for a single round and triggers re-registration
/// once the endpoint has been *continuously* unreachable for `reregister_after`.
///
/// Only *remote* TCP-connect outcomes are recorded here: a failure means the
/// server's listener port may be gone (cleanup, restart, etc.). Local-connect or
/// proxy errors (e.g. local server restarting) are deliberately not recorded,
/// avoiding spurious re-registration loops.
///
/// `last_success_ms` is the millisecond offset (from `round_start`) of the last
/// time the remote was known reachable, shared across all connections in the
/// round. It is refreshed both when a connection is established *and* when an
/// established connection ends — a live connection proves the remote was
/// reachable for as long as it lasted, so downtime is measured from when it
/// dropped, not from when it opened (which may be long ago for an idle tunnel).
/// Initialized to 0, i.e. "healthy at round start", so a never-reachable endpoint
/// still trips after `reregister_after`. Tracking time rather than a failure count
/// decouples the trigger from how many connections happen to fail at once.
#[derive(Clone)]
struct RoundHealth {
    round_start: Instant,
    last_success_ms: Arc<AtomicU64>,
    reregister_after: Duration,
    reregister_tx: mpsc::Sender<()>,
}

impl RoundHealth {
    fn new(reregister_after: Duration, reregister_tx: mpsc::Sender<()>) -> Self {
        Self {
            round_start: Instant::now(),
            last_success_ms: Arc::new(AtomicU64::new(0)),
            reregister_after,
            reregister_tx,
        }
    }

    fn record_success(&self) {
        self.last_success_ms.store(
            self.round_start.elapsed().as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    /// Record a remote-connect failure and request re-registration if the
    /// endpoint has now been down long enough. Returns the current downtime.
    fn record_failure(&self) -> Duration {
        let down_for_ms = (self.round_start.elapsed().as_millis() as u64)
            .saturating_sub(self.last_success_ms.load(Ordering::Relaxed));
        if down_for_ms >= self.reregister_after.as_millis() as u64 {
            let _ = self.reregister_tx.try_send(());
        }
        Duration::from_millis(down_for_ms)
    }
}

fn start_tunnel_connections(
    server: &TunnelServerInfo,
    local_host: Option<String>,
    local_port: u16,
    shutdown_signal: broadcast::Sender<()>,
    max_conn: u8,
    health: RoundHealth,
) {
    let server_host = server.host.clone();
    let server_port = server.port;
    let local_host = local_host.unwrap_or_else(|| LOCAL_HOST.to_string());

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
                    let health = health.clone();
                    let mut shutdown_receiver = shutdown_signal.subscribe();

                    tokio::spawn(async move {
                        tokio::select! {
                            _ = tunnel_one_connection(
                                &server_host, server_port,
                                &local_host, local_port,
                                &health,
                            ) => {}
                            _ = shutdown_receiver.recv() => {
                                log::info!("Shutting down connection");
                            }
                        }

                        drop(permit);
                    });
                }
                _ = shutdown_receiver.recv() => {
                    log::info!("Shutting down the loop");
                    return;
                }
            };
        }
    });
}

async fn tunnel_one_connection(
    server_host: &str,
    server_port: u16,
    local_host: &str,
    local_port: u16,
    health: &RoundHealth,
) {
    log::debug!("Connecting to remote: {}:{}", server_host, server_port);
    let remote_stream = match TcpStream::connect(format!("{server_host}:{server_port}")).await {
        Ok(stream) => {
            health.record_success();
            stream
        }
        Err(err) => {
            let down_for = health.record_failure();
            log::error!("Remote connect failed (down for {:?}): {:?}", down_for, err);
            sleep(Duration::from_secs(10)).await;
            return;
        }
    };

    let proxy_result = proxy_through(remote_stream, local_host, local_port).await;
    // The remote stayed reachable for the whole life of this connection, which
    // just ended. Refresh the timestamp so that if reconnects now start failing,
    // downtime is measured from this moment rather than from when the connection
    // was first opened — otherwise an idle tunnel that drops would report a huge
    // downtime on the very first failure and re-register on a momentary blip.
    health.record_success();
    if let Err(err) = proxy_result {
        log::error!("Proxy error: {:?}", err);
        sleep(Duration::from_secs(10)).await;
    }
}

async fn proxy_through(
    mut remote_stream: TcpStream,
    local_host: &str,
    local_port: u16,
) -> Result<()> {
    log::debug!("Connecting to local: {}:{}", local_host, local_port);
    let mut local_stream = TcpStream::connect(format!("{local_host}:{local_port}")).await?;

    let ka = TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_TIME)
        .with_interval(TCP_KEEPALIVE_INTERVAL);
    #[cfg(not(target_os = "windows"))]
    let ka = ka.with_retries(TCP_KEEPALIVE_RETRIES);
    let sf = SockRef::from(&remote_stream);
    sf.set_tcp_keepalive(&ka)?;

    io::copy_bidirectional(&mut remote_stream, &mut local_stream).await?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    // A connection that was alive while the tunnel sat idle, then dropped, must
    // not be reported as a long outage: refreshing health when the connection
    // ends means the first reconnect failure measures downtime from the drop,
    // so a brief blip does not force a re-registration.
    #[tokio::test]
    async fn downtime_resets_when_connection_ends() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(200), tx);

        health.record_success(); // connection established
        sleep(Duration::from_millis(300)).await; // idle, alive, > window
        health.record_success(); // connection ends — remote was reachable until now

        let down = health.record_failure(); // first failure right after the drop
        assert!(down < Duration::from_millis(200), "downtime was {down:?}");
        assert!(
            rx.try_recv().is_err(),
            "must not re-register on a fresh drop after an idle period"
        );
    }

    // Sustained downtime (no success for longer than the window) must trigger
    // re-registration.
    #[tokio::test]
    async fn triggers_after_sustained_downtime() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(200), tx);

        health.record_success();
        sleep(Duration::from_millis(250)).await; // unreachable past the window

        let down = health.record_failure();
        assert!(down >= Duration::from_millis(200), "downtime was {down:?}");
        assert!(rx.try_recv().is_ok(), "should request re-registration");
    }
}
