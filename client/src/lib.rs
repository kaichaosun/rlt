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

/// How long a pooled socket must have survived before the remote closing it counts as
/// ordinary pool recycling rather than an endpoint failure. A socket closed within this
/// window never had a chance to carry a request: the server was restarting, its listener
/// was gone, or it refused the socket outright (`Reached sockets max` in
/// `server/src/state.rs`). A socket the server held for longer and then closed is
/// indistinguishable from a normal lifecycle event — a redeploy, the subdomain entry
/// being replaced, an idle timeout on a load balancer in front of it — and must not
/// force a re-registration. 5 s is short enough that a dead endpoint is detected within
/// one reconnect cycle, and long enough that no healthy pool ever falls under it.
const IDLE_CLOSE_MIN_LIFETIME: Duration = Duration::from_secs(5);

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

/// Which reconnect delay a finished connection task should apply. Phase 2 only decides
/// *which* counter is at fault; the actual delays still come from the hardcoded sleeps
/// and are replaced by real exponential backoff in phase 3. Keeping remote and local
/// apart matters because they have opposite tuning: a returning local service should be
/// picked up in milliseconds, an unreachable endpoint should be retried ever more slowly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackoffAction {
    /// The connection did its job — reset any backoff and reconnect at once.
    None,
    /// The remote endpoint is at fault. Back off the remote-side counter.
    Remote,
    /// The remote endpoint is fine, the local application is not. Back off the
    /// local-side counter; endpoint health is untouched.
    Local,
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
/// Health is recorded from the *outcome* of a connection, never from the mere fact that a
/// TCP connect succeeded: an endpoint that accepts a socket and closes it a moment later
/// is unusable, and counting that accept as a success would refresh the timestamp forever
/// so the trigger could never fire. Evidence of health is therefore traffic actually
/// served, or a pooled socket the remote kept for at least `IDLE_CLOSE_MIN_LIFETIME`
/// before closing it. Local-connect or proxy errors (e.g. the local server restarting)
/// are deliberately not recorded as failures — the remote is fine, only the developer's
/// application is down — and drive their own backoff instead, avoiding spurious
/// re-registration loops.
///
/// `last_success_ms` is the millisecond offset (from `round_start`) of the last time the
/// remote was known usable, shared across all connections in the round. It is refreshed
/// when a connection ends after proving the remote was usable — a live connection proves
/// reachability for as long as it lasted, so downtime is measured from when it dropped,
/// not from when it opened (which may be long ago for an idle tunnel). Initialized to 0,
/// i.e. "healthy at round start", so a never-usable endpoint still trips after
/// `reregister_after`. Tracking time rather than a failure count decouples the trigger
/// from how many connections happen to fail at once.
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

    /// Fold a finished connection's outcome into endpoint health and report which
    /// reconnect delay the caller should apply.
    ///
    /// Only the remote endpoint's own behaviour moves the health timestamp. A socket the
    /// remote accepted and then closed before it could carry anything is a failure even
    /// though the TCP connect succeeded; a socket it kept for a while and then closed is
    /// ordinary recycling. A local outage is never charged to the endpoint.
    fn record_outcome(&self, outcome: &ConnOutcome) -> BackoffAction {
        match outcome {
            ConnOutcome::Served { bytes } => {
                log::debug!("Connection served {} bytes", bytes);
                self.record_success();
                BackoffAction::None
            }
            ConnOutcome::RemoteConnectFailed => {
                let down_for = self.record_failure();
                log::warn!("Remote endpoint unreachable (down for {:?})", down_for);
                BackoffAction::Remote
            }
            ConnOutcome::RemoteClosedIdle { lifetime } if *lifetime < IDLE_CLOSE_MIN_LIFETIME => {
                let down_for = self.record_failure();
                log::warn!(
                    "Remote closed an idle socket after {:?} (down for {:?})",
                    lifetime,
                    down_for
                );
                BackoffAction::Remote
            }
            ConnOutcome::RemoteClosedIdle { lifetime } => {
                log::debug!("Remote recycled an idle socket after {:?}", lifetime);
                self.record_success();
                BackoffAction::None
            }
            ConnOutcome::LocalUnavailable => {
                log::error!("Local service unavailable, remote endpoint left untouched");
                self.record_success();
                BackoffAction::Local
            }
        }
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
        // A successful connect proves nothing on its own: the server may hand back a
        // socket it is about to drop. Only the outcome of the connection moves health.
        Ok(remote_stream) => match proxy_through(remote_stream, local_host, local_port).await {
            Ok(outcome) => outcome,
            // A genuine I/O error mid-splice still proves the remote was reachable, so it
            // must not be charged to endpoint health; treat it as a local-side problem.
            Err(err) => {
                log::error!("Proxy error: {:?}", err);
                ConnOutcome::LocalUnavailable
            }
        },
        Err(err) => {
            log::error!("Remote connect failed: {:?}", err);
            ConnOutcome::RemoteConnectFailed
        }
    };

    let action = health.record_outcome(&outcome);

    // Accounting and pacing are deliberately decided separately. The delays below are
    // unchanged from the previous phase: an endpoint that closes idle sockets must still
    // be retried twice a second, because that is what refills the pool the moment it
    // recovers. Waiting ten seconds here would pin every semaphore permit taken in
    // `start_tunnel_connections` and leave the tunnel dead for the whole recovery window.
    // `action` already separates a remote fault from a local one; the next phase turns
    // that distinction into two independent exponential backoff counters and retires
    // both constants below.
    match outcome {
        ConnOutcome::Served { .. } => {}
        ConnOutcome::RemoteClosedIdle { .. } => sleep(RECONNECT_AFTER_IDLE_CLOSE).await,
        ConnOutcome::LocalUnavailable | ConnOutcome::RemoteConnectFailed => {
            log::debug!("Reconnect delayed after {:?}", action);
            sleep(Duration::from_secs(10)).await
        }
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

    // A connection that actually carried traffic is the strongest possible proof that
    // the endpoint is alive, so it clears any accumulated downtime and asks for no delay.
    #[tokio::test]
    async fn served_connection_marks_endpoint_healthy() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(200), tx);

        sleep(Duration::from_millis(250)).await; // past the window
        let action = health.record_outcome(&ConnOutcome::Served { bytes: 42 });

        assert_eq!(
            action,
            BackoffAction::None,
            "served traffic needs no backoff"
        );
        assert!(
            rx.try_recv().is_err(),
            "a served connection must not re-register"
        );
        let down = health.record_failure();
        assert!(down < Duration::from_millis(200), "downtime was {down:?}");
    }

    // A refused TCP connect is the classic "the listener is gone" signal and is the one
    // case that already worked before phase 2; it must keep working.
    #[tokio::test]
    async fn remote_connect_failure_counts_against_the_endpoint() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(200), tx);

        sleep(Duration::from_millis(250)).await;
        let action = health.record_outcome(&ConnOutcome::RemoteConnectFailed);

        assert_eq!(
            action,
            BackoffAction::Remote,
            "a dead endpoint needs remote backoff"
        );
        assert!(
            rx.try_recv().is_ok(),
            "sustained connect failures must re-register"
        );
    }

    // The defect this phase exists for: the server accepts the socket and drops it right
    // away, so `TcpStream::connect` succeeds and nothing used to be recorded. A socket
    // closed this fast never had a chance to serve anything — it is an endpoint failure.
    #[tokio::test]
    async fn instant_remote_close_counts_as_endpoint_failure() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(200), tx);

        sleep(Duration::from_millis(250)).await;
        let action = health.record_outcome(&ConnOutcome::RemoteClosedIdle {
            lifetime: Duration::from_millis(1),
        });

        assert_eq!(
            action,
            BackoffAction::Remote,
            "an instant close is a failure"
        );
        assert!(
            rx.try_recv().is_ok(),
            "instant closes must arm re-registration"
        );
    }

    // The opposite end of the same variant: a socket the server held for a while and then
    // closed is ordinary recycling (redeploy, subdomain entry replaced, LB idle timeout).
    // Charging it to the endpoint would re-register a perfectly healthy tunnel. The
    // threshold is inclusive, so a lifetime exactly at the boundary counts as healthy.
    #[tokio::test]
    async fn long_lived_remote_close_is_normal_pool_recycling() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(200), tx);

        sleep(Duration::from_millis(250)).await;
        let action = health.record_outcome(&ConnOutcome::RemoteClosedIdle {
            lifetime: IDLE_CLOSE_MIN_LIFETIME,
        });

        assert_eq!(
            action,
            BackoffAction::None,
            "pool recycling needs no remote backoff"
        );
        assert!(
            rx.try_recv().is_err(),
            "pool recycling must not re-register"
        );
    }

    // A local outage says nothing about the remote endpoint; re-registering would move
    // the listener for no reason and loop for as long as the developer's service is down.
    // Repeating it well past the window must still leave the endpoint marked healthy.
    #[tokio::test]
    async fn local_outage_never_reregisters() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(200), tx);

        for _ in 0..3 {
            sleep(Duration::from_millis(100)).await;
            let action = health.record_outcome(&ConnOutcome::LocalUnavailable);
            assert_eq!(
                action,
                BackoffAction::Local,
                "local outages back off locally"
            );
        }

        assert!(
            rx.try_recv().is_err(),
            "a local outage must never trigger re-registration, however long it lasts",
        );
    }

    // Guards the load-bearing decision of plan 02-02: a successful TCP connect is no
    // longer evidence of health. Nothing inside an instant-close loop may refresh
    // `last_success_ms`, otherwise downtime is wiped on every iteration and the trigger
    // can never fire — which is precisely why the tunnel stayed dead in issue #8. Unlike
    // the tests above, the window here cannot be cleared by a single step, so the
    // accumulation itself is exercised rather than assumed.
    #[tokio::test]
    async fn repeated_instant_closes_accumulate_downtime() {
        let (tx, mut rx) = mpsc::channel(1);
        let health = RoundHealth::new(Duration::from_millis(500), tx);

        for _ in 0..2 {
            sleep(Duration::from_millis(100)).await;
            let action = health.record_outcome(&ConnOutcome::RemoteClosedIdle {
                lifetime: Duration::from_millis(1),
            });
            assert_eq!(
                action,
                BackoffAction::Remote,
                "instant closes back off remotely"
            );
            assert!(
                rx.try_recv().is_err(),
                "downtime is still inside the window"
            );
        }

        sleep(Duration::from_millis(400)).await;
        health.record_outcome(&ConnOutcome::RemoteClosedIdle {
            lifetime: Duration::from_millis(1),
        });

        assert!(
            rx.try_recv().is_ok(),
            "instant closes must accumulate downtime, not reset it on every attempt",
        );
    }
}
