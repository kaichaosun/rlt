use std::time::Duration;

use localtunnel_client::{broadcast, open_tunnel, ClientConfig, NOISE_PARAMS};
use localtunnel_server::{start, ServerConfig};
use snowstorm::{Builder, NoiseStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, Instant};

async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Minimal HTTP/1.1 app behind the tunnel.
async fn spawn_local_http(body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

async fn spawn_server() -> (u16, u16) {
    let api_port = free_port().await;
    let proxy_port = free_port().await;
    // `start` drives actix's non-Send server future, so it gets its own
    // runtime on a dedicated thread instead of `tokio::spawn`.
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(start(ServerConfig {
                domain: "127.0.0.1".to_string(),
                api_port,
                secure: false,
                max_sockets: 4,
                proxy_port,
                require_auth: false,
            }))
            .unwrap();
    });

    // Wait until the API server accepts connections.
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", api_port)).await.is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        sleep(Duration::from_millis(50)).await;
    }
    (api_port, proxy_port)
}

#[tokio::test]
async fn proxies_request_through_encrypted_tunnel() {
    let body = "hello through the tunnel";
    let local_port = spawn_local_http(body).await;
    let (api_port, proxy_port) = spawn_server().await;

    let (shutdown_tx, _) = broadcast::channel(1);
    let url = open_tunnel(ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("e2e".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 4,
        credential: None,
        reregister_after: None,
    })
    .await
    .unwrap();
    assert_eq!(url, "http://e2e.127.0.0.1");

    // Tunnel connections are pooled asynchronously; retry until one serves us.
    let deadline = Instant::now() + Duration::from_secs(10);
    let response = loop {
        let mut conn = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        conn.write_all(b"GET / HTTP/1.1\r\nHost: e2e.127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = String::new();
        let _ = conn.read_to_string(&mut buf).await;
        if buf.contains("200 OK") {
            break buf;
        }
        assert!(
            Instant::now() < deadline,
            "no successful proxied response, last: {buf:?}"
        );
        sleep(Duration::from_millis(100)).await;
    };
    assert!(response.ends_with(body), "unexpected response: {response:?}");

    let _ = shutdown_tx.send(());
}

#[derive(serde::Deserialize)]
struct Registered {
    port: u16,
    server_public_key: String,
    session_token: String,
}

#[tokio::test]
async fn unauthenticated_tunnel_connections_are_rejected() {
    let (api_port, _proxy_port) = spawn_server().await;

    // Register a tunnel directly against the API to learn the assigned port.
    let reg: Registered = reqwest::get(format!("http://127.0.0.1:{api_port}/reject"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // A peer that speaks framed garbage instead of a Noise handshake must be
    // dropped without ever joining the connection pool.
    let mut conn = TcpStream::connect(("127.0.0.1", reg.port)).await.unwrap();
    conn.write_all(&[8u8, 0]).await.unwrap(); // frame length: 8
    conn.write_all(&[0u8; 8]).await.unwrap(); // not a valid handshake message
    let mut buf = [0u8; 16];
    assert!(
        matches!(conn.read(&mut buf).await, Ok(0) | Err(_)),
        "garbage connection should be closed"
    );

    // A peer with the right server key but the wrong session token completes
    // the handshake yet must be rejected before receiving the ack byte.
    let server_key = hex::decode(&reg.server_public_key).unwrap();
    let mut wrong_token = hex::decode(&reg.session_token).unwrap();
    wrong_token[0] ^= 0xff;

    let initiator = Builder::new(NOISE_PARAMS.parse().unwrap())
        .remote_public_key(&server_key)
        .build_initiator()
        .unwrap();
    let conn = TcpStream::connect(("127.0.0.1", reg.port)).await.unwrap();
    let mut stream = NoiseStream::handshake(conn, initiator).await.unwrap();
    stream.write_all(&wrong_token).await.unwrap();
    stream.flush().await.unwrap();

    let mut ack = [0u8; 1];
    assert!(
        stream.read_exact(&mut ack).await.is_err(),
        "server must not ack a wrong session token"
    );
}
