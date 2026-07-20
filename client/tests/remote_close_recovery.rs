use std::sync::{
    atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering},
    Arc,
};

use localtunnel_client::{broadcast, open_tunnel, ClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration};

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

// Accepts and then *holds* every connection open, never closing it. This models a
// real HTTP server sitting on an idle keep-alive connection, and it is a necessary
// condition for reproducing the stall: as long as the local side never sends EOF,
// a bidirectional copy that waits for both directions can never return.
async fn local_keepalive(listener: TcpListener, accepts: Arc<AtomicU32>) {
    let mut held = Vec::new();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                accepts.fetch_add(1, Ordering::Relaxed);
                held.push(stream);
            }
            Err(_) => return,
        }
    }
}

// While `closing` is true every accepted socket is dropped immediately (FIN), which
// is exactly what the server does today on the `Reached sockets max` path in
// `server/src/state.rs` and while it restarts. Once `closing` flips to false the
// remote is healthy again: sockets are counted and pooled.
async fn flaky_remote(listener: TcpListener, closing: Arc<AtomicBool>, pooled: Arc<AtomicU32>) {
    let mut held = Vec::new();
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        if closing.load(Ordering::Relaxed) {
            drop(stream);
        } else {
            pooled.fetch_add(1, Ordering::Relaxed);
            held.push(stream);
        }
    }
}

// Accepts, counts and holds sockets without ever writing anything into them. This is
// the "socket parked in the server's pool, no request yet" state.
async fn holding_remote(listener: TcpListener, pooled: Arc<AtomicU32>) {
    let mut held = Vec::new();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                pooled.fetch_add(1, Ordering::Relaxed);
                held.push(stream);
            }
            Err(_) => return,
        }
    }
}

// This is the issue #8 regression test and it fails on f86f32b. There, the remote
// closing a pooled socket makes the remote -> local direction hit EOF, but the live
// local keep-alive connection never produces the second EOF, so the bidirectional
// copy future never completes. The connection task never returns, its semaphore
// permit is never released, all max_conn permits leak and no new socket is ever
// offered — `pooled` stays 0 forever, even after the remote is healthy again.
#[tokio::test(flavor = "multi_thread")]
async fn pool_recovers_after_remote_closes_every_socket() {
    let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = local.local_addr().unwrap().port();
    let local_accepts = Arc::new(AtomicU32::new(0));
    tokio::spawn(local_keepalive(local, local_accepts));

    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = remote.local_addr().unwrap().port();
    let closing = Arc::new(AtomicBool::new(true));
    let pooled = Arc::new(AtomicU32::new(0));
    tokio::spawn(flaky_remote(remote, closing.clone(), pooled.clone()));

    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(mock_api_server(api, Arc::new(AtomicU16::new(remote_port))));

    // `reregister_after: None` on purpose: recovery must be proven *without* the
    // re-registration mechanism, otherwise this test measures the wrong thing.
    let (shutdown_tx, _) = broadcast::channel(1);
    let config = ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("test".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 10,
        credential: None,
        reregister_after: None,
    };
    open_tunnel(config).await.unwrap();

    // Phase 1: the remote is down and closes every socket it accepts.
    sleep(Duration::from_secs(3)).await;

    // Phase 2: the remote is healthy again and pools sockets.
    closing.store(false, Ordering::Relaxed);

    tokio::time::timeout(Duration::from_secs(5), async {
        while pooled.load(Ordering::Relaxed) == 0 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("tunnel must offer sockets again within 5s after the remote recovered");

    let _ = shutdown_tx.send(());
}

// A socket parked in the server's pool has no request on it yet, so there is nothing
// for the local service to answer. Dialing the local service before the first byte
// arrives burns a local connection (and a local server slot) per pooled socket. On
// f86f32b the local connect happens eagerly, so this counts max_conn accepts.
#[tokio::test(flavor = "multi_thread")]
async fn no_local_connection_before_first_byte() {
    let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = local.local_addr().unwrap().port();
    let local_accepts = Arc::new(AtomicU32::new(0));
    tokio::spawn(local_keepalive(local, local_accepts.clone()));

    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = remote.local_addr().unwrap().port();
    let pooled = Arc::new(AtomicU32::new(0));
    tokio::spawn(holding_remote(remote, pooled.clone()));

    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(mock_api_server(api, Arc::new(AtomicU16::new(remote_port))));

    let (shutdown_tx, _) = broadcast::channel(1);
    let config = ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("test".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 10,
        credential: None,
        reregister_after: None,
    };
    open_tunnel(config).await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while pooled.load(Ordering::Relaxed) == 0 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("client should offer sockets to the remote");

    // Give an eager local connect more than enough time to show up.
    sleep(Duration::from_secs(1)).await;

    assert_eq!(
        local_accepts.load(Ordering::Relaxed),
        0,
        "client must not dial the local service before the first request byte arrives, got {} accepts",
        local_accepts.load(Ordering::Relaxed)
    );

    let _ = shutdown_tx.send(());
}
