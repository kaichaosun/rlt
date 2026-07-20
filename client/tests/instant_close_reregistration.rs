use std::sync::{
    atomic::{AtomicU16, AtomicU32, Ordering},
    Arc,
};

use localtunnel_client::{broadcast, open_tunnel, ClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration, Instant};

// Same mock API as `reregistration.rs`, plus a request counter: re-registration is
// observable only as "the client asked the API for an endpoint again".
async fn mock_api_server(
    listener: TcpListener,
    endpoint_port: Arc<AtomicU16>,
    requests: Arc<AtomicU32>,
) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(value) => value,
            Err(_) => return,
        };
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;
        requests.fetch_add(1, Ordering::Relaxed);

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

// Remote endpoint that accepts and immediately drops every socket: this is what a
// restarting server, and the `Reached sockets max` path in `server/src/state.rs`,
// look like from the client side.
async fn accept_and_close(listener: TcpListener, accepted: Arc<AtomicU32>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                accepted.fetch_add(1, Ordering::Relaxed);
                drop(stream);
            }
            Err(_) => return,
        }
    }
}

// Healthy remote endpoint: it holds every pooled socket and, once `poke_after` has
// elapsed, pushes a minimal HTTP request into each one. That is what forces the client
// to dial the (absent) local service, i.e. it is the only way to exercise
// `LocalUnavailable` now that the local connect is lazy.
async fn accept_hold_and_poke(
    listener: TcpListener,
    accepted: Arc<AtomicU32>,
    poke_after: Duration,
) {
    let mut held = Vec::new();
    let mut poke_at = Instant::now() + poke_after;
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                match incoming {
                    Ok((stream, _)) => {
                        accepted.fetch_add(1, Ordering::Relaxed);
                        held.push(stream);
                    }
                    Err(_) => return,
                }
            }
            _ = tokio::time::sleep_until(poke_at) => {
                for stream in held.iter_mut() {
                    let _ = stream.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n").await;
                }
                // Poke exactly once; push the deadline far out so the arm stops firing.
                poke_at = Instant::now() + Duration::from_secs(3600);
            }
        }
    }
}

// A remote endpoint that accepts and instantly closes every socket is *not* a healthy
// endpoint: the tunnel can never carry traffic through it. Until the outcome of a
// connection feeds back into `RoundHealth`, `record_failure()` is never reached (the
// TCP connect itself succeeds), so the re-registration timer never arms and the tunnel
// stays dead — exactly the "restarting the service doesn't help" report in issue #8.
#[tokio::test(flavor = "multi_thread")]
async fn reregisters_when_remote_closes_every_socket() {
    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = remote.local_addr().unwrap().port();
    let remote_accepted = Arc::new(AtomicU32::new(0));
    tokio::spawn(accept_and_close(remote, remote_accepted.clone()));

    // No local service is needed: the remote never delivers a request byte.
    let dead_local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = dead_local.local_addr().unwrap().port();
    drop(dead_local);

    let api_requests = Arc::new(AtomicU32::new(0));
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(mock_api_server(
        api,
        Arc::new(AtomicU16::new(remote_port)),
        api_requests.clone(),
    ));

    // `Duration::ZERO` makes the very first recorded failure trip the re-registration
    // window, the same trick `reregistration.rs` uses: the test must never wait for
    // a product constant (30 s window, 10 s reconnect sleep). The windowing arithmetic
    // itself is covered by the unit tests in `client/src/lib.rs`.
    let (shutdown_tx, _) = broadcast::channel(1);
    let config = ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("test".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 10,
        credential: None,
        reregister_after: Some(Duration::ZERO),
        reconnect_base_delay: None,
        reconnect_max_delay: None,
    };
    open_tunnel(config).await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while remote_accepted.load(Ordering::Relaxed) == 0 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("client should offer at least one socket to the remote endpoint");

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        while api_requests.load(Ordering::Relaxed) < 2 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    let _ = shutdown_tx.send(());
    outcome.expect(
        "an endpoint that accepts and instantly closes every socket must be treated as \
         unreachable and trigger re-registration",
    );
}

// The mirror image: the remote endpoint is perfectly healthy, only the local
// application is down. Re-registering would move the public URL's listener for no
// reason and would loop forever while the developer restarts their service, so a local
// outage must never be charged to the remote endpoint — see the doc comment on
// `RoundHealth` in `client/src/lib.rs`.
#[tokio::test(flavor = "multi_thread")]
async fn local_outage_does_not_trigger_reregistration() {
    // Bind then drop: a port nothing listens on, i.e. the local service is down.
    let dead_local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = dead_local.local_addr().unwrap().port();
    drop(dead_local);

    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = remote.local_addr().unwrap().port();
    let remote_accepted = Arc::new(AtomicU32::new(0));
    // Poke after 400 ms — well past the 200 ms re-registration window below, so a
    // mis-classified `LocalUnavailable` would trip the trigger and fail this test.
    tokio::spawn(accept_hold_and_poke(
        remote,
        remote_accepted.clone(),
        Duration::from_millis(400),
    ));

    let api_requests = Arc::new(AtomicU32::new(0));
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(mock_api_server(
        api,
        Arc::new(AtomicU16::new(remote_port)),
        api_requests.clone(),
    ));

    let (shutdown_tx, _) = broadcast::channel(1);
    let config = ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("test".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 10,
        credential: None,
        reregister_after: Some(Duration::from_millis(200)),
        reconnect_base_delay: None,
        reconnect_max_delay: None,
    };
    open_tunnel(config).await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while remote_accepted.load(Ordering::Relaxed) == 0 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("client should offer sockets to the healthy remote endpoint");

    // Let the poke land and the client fail its local connect several times over.
    sleep(Duration::from_secs(2)).await;
    let requests = api_requests.load(Ordering::Relaxed);
    let _ = shutdown_tx.send(());

    assert_eq!(
        requests, 1,
        "a local-service outage must not re-register the remote endpoint, got {requests} API requests",
    );
}
