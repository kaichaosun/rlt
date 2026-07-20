use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpStream;
pub use tokio::sync::broadcast;
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout, Duration};

pub const PROXY_SERVER: &str = "https://your-domain.com";
pub const LOCAL_HOST: &str = "127.0.0.1";

// See https://tldp.org/HOWTO/html_single/TCP-Keepalive-HOWTO to understand how keepalive work.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(not(target_os = "windows"))]
const TCP_KEEPALIVE_RETRIES: u32 = 5;

/// Size of the buffer used to wait for the first byte of a request. One byte is
/// enough because the read is a *detector* ("data arrived" vs "FIN arrived"), not
/// a parser — the bytes are peeked, not consumed, and the splice reads them again.
/// The buffer must not be empty: peeking into an empty slice returns `Ok(0)` and
/// would look like a false EOF.
const FIRST_BYTE_PROBE: usize = 1;

/// Upper bound on "let the other direction drain its tail" after one direction has
/// already reached EOF. It is only ever armed *after* an EOF, so a connection with
/// both halves alive (a websocket, an SSE stream) is never affected by it: neither
/// copy completes, so the grace timeout is not even created.
const HALF_CLOSE_GRACE: Duration = Duration::from_secs(10);

/// Pause before re-offering a socket after the remote closed an idle pooled one.
/// This is a safety belt, not decoration: before this fix such a task hung forever,
/// now it finishes in milliseconds, and without a pause `max_conn` tasks would spin
/// in a hot reconnect loop while the endpoint keeps refusing sockets. 500 ms caps
/// that at two attempts per second per task while still fitting inside the 5 s
/// recovery window the regression test asserts. Phase 3 replaces this constant with
/// exponential backoff plus deterministic jitter.
const RECONNECT_AFTER_IDLE_CLOSE: Duration = Duration::from_millis(500);

/// How a single tunnel connection ended.
///
/// Modelled as an explicit outcome rather than `Result<()>` because the interesting
/// cases are not errors: a remote that closes a pooled socket it no longer needs, and
/// a local service that is momentarily down, are both normal states of a tunnel. Only
/// genuine I/O failures stay in `Err`. Phase 2 maps these variants onto `RoundHealth`,
/// which is why the distinction between a remote-side and a local-side failure has to
/// survive all the way up to the caller.
#[derive(Debug)]
enum ConnOutcome {
    /// Bytes flowed in at least one direction and the connection finished cleanly.
    /// `bytes` is the total moved in both directions.
    Served { bytes: u64 },
    /// The remote closed (FIN / RST / keepalive timeout) before sending a single
    /// request byte. `lifetime` is measured from the moment the socket entered
    /// `proxy_through` until the read returned 0; phase 2 compares it against
    /// `IDLE_CLOSE_MIN_LIFETIME` to tell "server went away" from routine pool churn.
    RemoteClosedIdle { lifetime: Duration },
    /// A request arrived but the local service refused the connection. Note this is a
    /// *local* failure — the remote endpoint is healthy, so it must never count
    /// against endpoint health (see the docs on [`RoundHealth`]).
    LocalUnavailable,
    /// The TCP connect to the endpoint itself failed. Produced by
    /// `tunnel_one_connection`, not by `proxy_through`.
    RemoteConnectFailed,
}

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
    let outcome = match TcpStream::connect(format!("{server_host}:{server_port}")).await {
        Ok(stream) => {
            health.record_success();
            let proxy_result = proxy_through(stream, local_host, local_port).await;
            // The remote stayed reachable for the whole life of this connection, which
            // just ended. Refresh the timestamp so that if reconnects now start failing,
            // downtime is measured from this moment rather than from when the connection
            // was first opened — otherwise an idle tunnel that drops would report a huge
            // downtime on the very first failure and re-register on a momentary blip.
            health.record_success();
            match proxy_result {
                Ok(outcome) => outcome,
                Err(err) => {
                    log::error!("Proxy error: {:?}", err);
                    sleep(Duration::from_secs(10)).await;
                    return;
                }
            }
        }
        Err(err) => {
            let down_for = health.record_failure();
            log::error!("Remote connect failed (down for {:?}): {:?}", down_for, err);
            ConnOutcome::RemoteConnectFailed
        }
    };

    match outcome {
        ConnOutcome::Served { bytes } => {
            log::debug!("Tunnel connection served {} bytes", bytes);
        }
        ConnOutcome::RemoteClosedIdle { lifetime } => {
            log::debug!("Remote closed an idle pooled socket after {:?}", lifetime);
            sleep(RECONNECT_AFTER_IDLE_CLOSE).await;
        }
        // The local service is down; without a pause the whole pool would turn into a
        // hot loop. This is the same 10 s that a local-connect error used to sleep for
        // when it surfaced as a proxy error. Phase 3 replaces it with a local backoff.
        ConnOutcome::LocalUnavailable => sleep(Duration::from_secs(10)).await,
        ConnOutcome::RemoteConnectFailed => sleep(Duration::from_secs(10)).await,
    }
}

fn set_remote_keepalive(stream: &TcpStream) -> Result<()> {
    let ka = TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_TIME)
        .with_interval(TCP_KEEPALIVE_INTERVAL);
    #[cfg(not(target_os = "windows"))]
    let ka = ka.with_retries(TCP_KEEPALIVE_RETRIES);
    let sf = SockRef::from(stream);
    sf.set_tcp_keepalive(&ka)?;
    Ok(())
}

/// Which half finished first, reported out of `select!` instead of acted on inside it.
enum FirstDone {
    RemoteToLocal(u64),
    LocalToRemote(u64),
    Failed(std::io::Error),
}

/// Splice the two streams with two half-copies, closing each write half explicitly
/// once its source has reached EOF. Returns the total number of bytes moved.
async fn splice_halves(remote_stream: &mut TcpStream, local_stream: &mut TcpStream) -> u64 {
    // Borrowing `split()` rather than `into_split()`: ownership of both `TcpStream`s
    // stays with the caller, there is no per-connection `Arc` allocation, and no
    // implicit FIN from `Drop for OwnedWriteHalf`.
    let (mut remote_reader, mut remote_writer) = remote_stream.split();
    let (mut local_reader, mut local_writer) = local_stream.split();

    // As long as both directions are alive neither `io::copy` ever completes, so
    // `select!` does not fire and the grace timeout below is never even created — a
    // silent-but-live websocket is not torn down by an idle period of any length.
    // The grace window only starts counting after one side has already sent EOF.
    let first = tokio::select! {
        result = io::copy(&mut remote_reader, &mut local_writer) => match result {
            Ok(bytes) => FirstDone::RemoteToLocal(bytes),
            Err(err) => FirstDone::Failed(err),
        },
        result = io::copy(&mut local_reader, &mut remote_writer) => match result {
            Ok(bytes) => FirstDone::LocalToRemote(bytes),
            Err(err) => FirstDone::Failed(err),
        },
    };

    // Only here are both futures dropped and the borrows on the halves released.
    // Calling `shutdown()` inside a `select!` arm would not compile (E0499): the
    // losing future still holds a `&mut` on its halves until the macro exits.
    // `into_split()` does not change that, so switching split styles does not help.
    // `io::copy` flushes on EOF but never shuts down, hence the explicit calls.
    match first {
        FirstDone::RemoteToLocal(bytes) => {
            let _ = local_writer.shutdown().await;
            let tail = timeout(
                HALF_CLOSE_GRACE,
                io::copy(&mut local_reader, &mut remote_writer),
            )
            .await;
            let _ = remote_writer.shutdown().await;
            bytes + tail.ok().and_then(|result| result.ok()).unwrap_or(0)
        }
        FirstDone::LocalToRemote(bytes) => {
            let _ = remote_writer.shutdown().await;
            let tail = timeout(
                HALF_CLOSE_GRACE,
                io::copy(&mut remote_reader, &mut local_writer),
            )
            .await;
            let _ = local_writer.shutdown().await;
            bytes + tail.ok().and_then(|result| result.ok()).unwrap_or(0)
        }
        FirstDone::Failed(err) => {
            // The remote connection itself was established, so this is not a
            // `RemoteConnectFailed`; it is just a connection that ended badly.
            log::debug!("Proxy copy failed: {:?}", err);
            let _ = local_writer.shutdown().await;
            let _ = remote_writer.shutdown().await;
            0
        }
    }
}

async fn proxy_through(
    mut remote_stream: TcpStream,
    local_host: &str,
    local_port: u16,
) -> Result<ConnOutcome> {
    let opened_at = Instant::now();

    // Keepalive goes on first, before anything else. This socket is about to sit in
    // the server's pool with no local peer attached, waiting for a request that may
    // never come; without keepalive a silently dropped NAT/firewall state would keep
    // it hanging forever. It also covers the case where the local connect below
    // disappears into a black hole (a SYN with no answer takes ~2 minutes on Linux).
    set_remote_keepalive(&remote_stream)?;

    // Wait for the first request byte with *no* connection to the local service. This
    // is the fix for the stall: previously a FIN on an idle pooled socket never ended
    // the task, because the bidirectional copy also waited for the live local
    // keep-alive connection to reach EOF, so the semaphore permit leaked forever.
    //
    // No timeout here on purpose: a pooled socket is allowed to wait arbitrarily long,
    // and a dead peer is detected by TCP keepalive (worst case 30 + 5x10 = 80 s).
    // Adding a timeout here would break the server's connection pool.
    let mut probe = [0u8; FIRST_BYTE_PROBE];
    match remote_stream.peek(&mut probe).await {
        Ok(0) => {
            return Ok(ConnOutcome::RemoteClosedIdle {
                lifetime: opened_at.elapsed(),
            })
        }
        Err(err) => {
            // RST, ConnectionReset or a keepalive TimedOut all mean the same thing to
            // us: release the permit and reconnect. No need to split them apart.
            log::debug!("Remote closed an idle socket: {:?}", err.kind());
            return Ok(ConnOutcome::RemoteClosedIdle {
                lifetime: opened_at.elapsed(),
            });
        }
        Ok(_) => {}
    }

    log::debug!("Connecting to local: {}:{}", local_host, local_port);
    let mut local_stream = match TcpStream::connect(format!("{local_host}:{local_port}")).await {
        Ok(stream) => stream,
        Err(err) => {
            log::error!("Local connect failed: {:?}", err);
            // Answer with a real HTTP error so the caller sees a 502 instead of an
            // empty read. Content-Length must match the 21-byte body exactly, or the
            // far end blocks waiting for the rest. Write errors are swallowed on
            // purpose: the remote may already be gone, which is not worth logging.
            let _ = remote_stream
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: 21\r\nConnection: close\r\n\r\nlocal service is down")
                .await;
            let _ = remote_stream.shutdown().await;
            return Ok(ConnOutcome::LocalUnavailable);
        }
    };

    // Nothing to replay: `peek` did not consume the probed bytes, they are still in
    // the kernel receive buffer and the splice below picks them up on its first read.
    let bytes = splice_halves(&mut remote_stream, &mut local_stream).await;
    Ok(ConnOutcome::Served { bytes })
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
