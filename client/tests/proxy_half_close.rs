use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};

use localtunnel_client::{broadcast, open_tunnel, ClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
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

// Hands every socket the client offers over to the test body, which then plays the
// role of the browser whose request the server pushed into the tunnel.
async fn pooling_remote(listener: TcpListener, sockets: mpsc::Sender<TcpStream>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        if sockets.send(stream).await.is_err() {
            return;
        }
    }
}

// A port nobody listens on: bind, read the port, drop the listener.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// Echoes back whatever it reads and never closes the connection. Models a websocket
// peer: both halves stay alive for the whole life of the connection.
async fn local_echo(listener: TcpListener) {
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
                    Ok(read) => {
                        if stream.write_all(&buf[..read]).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
}

// On f86f32b the client dials the local service *before* reading the request, gets a
// connection refused, returns Err and simply drops the remote socket. The caller on
// the other end sees an empty read instead of an error response.
#[tokio::test(flavor = "multi_thread")]
async fn local_unavailable_returns_502() {
    let local_port = free_port();

    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = remote.local_addr().unwrap().port();
    let (sockets_tx, mut sockets_rx) = mpsc::channel(16);
    tokio::spawn(pooling_remote(remote, sockets_tx));

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
        max_conn: 1,
        credential: None,
        reregister_after: None,
    };
    open_tunnel(config).await.unwrap();

    let mut socket = tokio::time::timeout(Duration::from_secs(5), sockets_rx.recv())
        .await
        .expect("client should offer a socket to the remote")
        .expect("channel closed");

    socket
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();

    let mut buf = vec![0u8; 1024];
    let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
        .await
        .expect("client must answer within 5s")
        .expect("read failed");

    assert!(
        read > 0,
        "client returned an empty read instead of an HTTP error response"
    );
    let response = String::from_utf8_lossy(&buf[..read]);
    assert!(
        response.starts_with("HTTP/1.1 502"),
        "expected a 502 status line, got {response:?}"
    );

    let _ = shutdown_tx.send(());
}

// Non-regression baseline: this test must pass *both* before and after the fix. If it
// goes green "too early", the test is not broken — that is its whole purpose.
//
// The idle period is 12 s, deliberately longer than HALF_CLOSE_GRACE (10 s). The grace
// window must never apply here: it only starts counting once one direction has already
// reached EOF, and in a live websocket-like connection neither direction ever does.
//
// The test is intentionally slow (~13 s) — that is the price of checking a grace window
// on the real clock. `tokio::time::pause()` is incompatible with tests over real sockets.
#[tokio::test(flavor = "multi_thread")]
async fn live_bidirectional_connection_survives_idle_period() {
    let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = local.local_addr().unwrap().port();
    tokio::spawn(local_echo(local));

    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = remote.local_addr().unwrap().port();
    let (sockets_tx, mut sockets_rx) = mpsc::channel(16);
    tokio::spawn(pooling_remote(remote, sockets_tx));

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
        max_conn: 1,
        credential: None,
        reregister_after: None,
    };
    open_tunnel(config).await.unwrap();

    let mut socket = tokio::time::timeout(Duration::from_secs(5), sockets_rx.recv())
        .await
        .expect("client should offer a socket to the remote")
        .expect("channel closed");

    socket.write_all(b"ping-1").await.unwrap();
    let mut buf = [0u8; 6];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut buf))
        .await
        .expect("first echo must come back within 5s")
        .expect("read failed");
    assert_eq!(&buf, b"ping-1", "the splice must be up in both directions");

    sleep(Duration::from_secs(12)).await;

    socket.write_all(b"ping-2").await.unwrap();
    let mut buf = [0u8; 6];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut buf))
        .await
        .expect("second echo must come back within 5s")
        .expect("read failed");
    assert_eq!(
        &buf, b"ping-2",
        "a connection with both halves alive must survive an idle period longer than the half-close grace"
    );

    let _ = shutdown_tx.send(());
}
