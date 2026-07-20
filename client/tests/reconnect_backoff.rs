use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU16, AtomicU32, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use localtunnel_client::{broadcast, open_tunnel, ClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration};

async fn mock_api_server(listener: TcpListener, endpoint_port: Arc<AtomicU16>) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;

        let port = endpoint_port.load(Ordering::Relaxed);
        let body = format!(
            r#"{{"id":"test","port":{port},"max_conn_count":10,"url":"http://test.127.0.0.1:{port}"}}"#,
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    }
}

/// A remote endpoint that accepts every socket and drops it at once: the server is
/// restarting, its pool is full, or it refuses the socket outright. Unlike a plain
/// counter this records *when* each socket was offered — the phase is about the interval
/// between reconnects growing, and a total says nothing about an interval.
async fn accept_and_close(listener: TcpListener, log: Arc<Mutex<Vec<Instant>>>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                log.lock()
                    .expect("accept log is not poisoned")
                    .push(Instant::now());
                drop(stream);
            }
            Err(_) => return,
        }
    }
}

/// A listener that holds every accepted socket open, standing in for a service that is
/// up but silent. Used where the mock must not contribute failures of its own.
async fn accept_hold(listener: TcpListener) {
    let mut held = Vec::new();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => held.push(stream),
            Err(_) => return,
        }
    }
}

/// Stands in for the tunnel server's socket pool: sockets the client offered and that are
/// waiting for a request. `accepted` counts every socket ever offered.
struct RemotePool {
    sockets: tokio::sync::Mutex<VecDeque<TcpStream>>,
    accepted: AtomicU32,
}

async fn pooled_remote(listener: TcpListener, pool: Arc<RemotePool>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                pool.accepted.fetch_add(1, Ordering::Relaxed);
                pool.sockets.lock().await.push_back(stream);
            }
            Err(_) => return,
        }
    }
}

/// Take a socket the client is holding in the pool, send a minimal request and read the
/// reply, exactly as the real server does when a browser hits the tunnel. Sockets the
/// client has already closed are discarded on the way. Returns `None` if no live socket
/// appeared within `wait` — that is precisely the "pool starved" condition.
async fn request_through_pool(pool: &RemotePool, wait: Duration) -> Option<String> {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        let socket = pool.sockets.lock().await.pop_front();
        let mut socket = match socket {
            Some(socket) => socket,
            None => {
                sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if socket
            .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .is_err()
        {
            continue;
        }
        // Read until the reply has a body rather than until EOF: the local service behind
        // the tunnel is a keep-alive server that answers and keeps the socket open, so
        // waiting for EOF would time out and throw away a perfectly good `200 OK`. A
        // `502 Bad Gateway` written by the client is a *live* socket too — the tunnel
        // answered — and it satisfies the same condition. Only an empty read or an I/O
        // error means the client had already dropped this socket.
        let mut response = Vec::new();
        let mut buf = [0u8; 4096];
        let read_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let remaining = read_deadline.saturating_duration_since(Instant::now());
            match timeout(remaining, socket.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(read)) => {
                    response.extend_from_slice(&buf[..read]);
                    let headers_done = response.windows(4).any(|window| window == b"\r\n\r\n");
                    if headers_done && !response.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        if response.is_empty() {
            continue;
        }
        return Some(String::from_utf8_lossy(&response).to_string());
    }
    None
}

/// Minimal HTTP/1.1 server answering `200 OK` with `tag` as the body. The returned handle
/// is what lets a test "kill" the service with `abort()`.
async fn local_http(port: u16, tag: &'static str) -> JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("the local service should bind its port");
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{tag}",
                                tag.len(),
                            );
                            if stream.write_all(response.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    })
}

/// A port number nobody is listening on: needed when the port has to be known *before*
/// the service that will own it is started.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("binding an ephemeral port should succeed")
        .local_addr()
        .expect("a bound listener has a local address")
        .port()
}

// A remote that keeps refusing sockets must get cheaper to retry, not just be retried
// forever at a fixed rate. The test compares the density of reconnects in an early and a
// late window of the same run: a flat delay makes the two windows look alike, growth
// empties the late one. A total count cannot tell those apart, which is why this asserts
// on the distribution in time instead.
#[tokio::test(flavor = "multi_thread")]
async fn reconnect_attempts_slow_down_while_the_remote_keeps_closing_sockets() {
    let local = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("local mock should bind");
    let local_port = local.local_addr().unwrap().port();
    tokio::spawn(accept_hold(local));

    let remote = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("remote mock should bind");
    let remote_port = remote.local_addr().unwrap().port();
    let remote_accepts = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(accept_and_close(remote, remote_accepts.clone()));

    let api = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("api mock should bind");
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(mock_api_server(api, Arc::new(AtomicU16::new(remote_port))));

    // The re-registration window is deliberately far out of reach: this test measures how
    // often a connection task reconnects, not whether the tunnel re-registers. The backoff
    // bounds are given explicitly so the test never depends on the shipped defaults.
    let (shutdown_tx, _) = broadcast::channel(1);
    let config = ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("test".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 10,
        credential: None,
        reregister_after: Some(Duration::from_secs(60)),
        reconnect_base_delay: Some(Duration::from_millis(500)),
        reconnect_max_delay: Some(Duration::from_secs(30)),
    };
    open_tunnel(config).await.expect("tunnel should register");
    let started = Instant::now();

    sleep(Duration::from_secs(6)).await;
    let _ = shutdown_tx.send(());

    let stamps: Vec<Duration> = {
        let log = remote_accepts.lock().expect("accept log is not poisoned");
        log.iter()
            .map(|at| at.saturating_duration_since(started))
            .collect()
    };
    let early = stamps
        .iter()
        .filter(|elapsed| **elapsed < Duration::from_millis(1500))
        .count();
    let late = stamps
        .iter()
        .filter(|elapsed| {
            **elapsed >= Duration::from_millis(4500) && **elapsed < Duration::from_millis(5500)
        })
        .count();

    // A pool of ten tasks retries at the base delay while the failure is fresh: anything
    // near ten means a task connected once and then sat on a multi-second fixed sleep.
    assert!(
        early >= 15,
        "only {early} reconnects in the first 1.5 s (timeline: {stamps:?}) — \
         the base reconnect delay is not being used"
    );
    // The point of the phase: the same failure repeated must get cheaper over time. With a
    // flat delay this window looks exactly like the early one.
    assert!(
        late <= 3,
        "{late} reconnects between 4.5 s and 5.5 s (early window had {early}) — \
         the reconnect interval is not growing, the delay is still flat"
    );
    assert!(
        stamps.len() <= 200,
        "reconnects are spinning: {} attempts in six seconds",
        stamps.len()
    );
}

// A local service that is down says nothing about the remote endpoint, so the pool of
// offered sockets must stay populated throughout the outage (the tunnel answers 502
// instead of refusing the connection), and the moment the service is back the very next
// request must be served — in well under the ten seconds this phase replaces.
#[tokio::test(flavor = "multi_thread")]
async fn local_outage_does_not_starve_the_pool_and_recovers_fast() {
    let local_port = free_port();

    let remote = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("remote mock should bind");
    let remote_port = remote.local_addr().unwrap().port();
    let pool = Arc::new(RemotePool {
        sockets: tokio::sync::Mutex::new(VecDeque::new()),
        accepted: AtomicU32::new(0),
    });
    tokio::spawn(pooled_remote(remote, pool.clone()));

    let api = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("api mock should bind");
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(mock_api_server(api, Arc::new(AtomicU16::new(remote_port))));

    // The ceiling is set explicitly and low: this test must never wait on the shipped
    // 30 s default. That the sequence really climbs to 30 s is covered by the unit test
    // `backoff_never_exceeds_the_cap`.
    let (shutdown_tx, _) = broadcast::channel(1);
    let config = ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("test".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 5,
        credential: None,
        reregister_after: Some(Duration::from_secs(60)),
        reconnect_base_delay: None,
        reconnect_max_delay: Some(Duration::from_secs(2)),
    };
    open_tunnel(config).await.expect("tunnel should register");

    timeout(Duration::from_secs(5), async {
        while pool.accepted.load(Ordering::Relaxed) < 5 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the client should fill the remote pool");

    let mut starved = 0;
    for _ in 0..10 {
        sleep(Duration::from_millis(500)).await;
        if request_through_pool(&pool, Duration::from_secs(1))
            .await
            .is_none()
        {
            starved += 1;
        }
    }

    let _local = local_http(local_port, "V2").await;
    let restarted_at = Instant::now();
    let mut recovered = None;
    while restarted_at.elapsed() < Duration::from_millis(1500) {
        if let Some(response) = request_through_pool(&pool, Duration::from_millis(300)).await {
            if response.contains("200 OK") && response.contains("V2") {
                recovered = Some(restarted_at.elapsed());
                break;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    let recovered = recovered
        .expect("the local service came back but the tunnel did not pick it up within 1.5 s");
    assert_eq!(
        starved, 0,
        "the pool ran dry while only the local service was down (recovered after {recovered:?})"
    );

    let _ = shutdown_tx.send(());
}
