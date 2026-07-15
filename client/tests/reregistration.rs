use std::sync::{
    atomic::{AtomicU16, AtomicU32, Ordering},
    Arc,
};

use localtunnel_client::{broadcast, open_tunnel, ClientConfig, NOISE_PARAMS};
use snowstorm::{Builder, Keypair, NoiseStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration};

const SESSION_TOKEN: [u8; 32] = [7u8; 32];

async fn mock_api_server(listener: TcpListener, endpoint_port: Arc<AtomicU16>, public_key: String) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;

        let port = endpoint_port.load(Ordering::Relaxed);
        let token = hex::encode(SESSION_TOKEN);
        let body = format!(
            r#"{{"id":"test","port":{port},"max_conn_count":10,"url":"http://test.127.0.0.1:{port}","server_public_key":"{public_key}","session_token":"{token}"}}"#,
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    }
}

async fn accept_and_count(listener: TcpListener, counter: Arc<AtomicU32>) {
    loop {
        match listener.accept().await {
            Ok((_stream, _)) => {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => return,
        }
    }
}

// A tunnel endpoint that completes the encrypted handshake (so the client
// counts the endpoint as healthy) and then drops the stream, mirroring the
// old drop-on-accept behaviour of the plain-TCP mock.
async fn noise_accept_and_count(listener: TcpListener, counter: Arc<AtomicU32>, key: Arc<Keypair>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                counter.fetch_add(1, Ordering::Relaxed);
                let key = key.clone();
                tokio::spawn(async move {
                    let responder = Builder::new(NOISE_PARAMS.parse().unwrap())
                        .local_private_key(&key.private)
                        .build_responder()
                        .unwrap();
                    if let Ok(mut stream) = NoiseStream::handshake(stream, responder).await {
                        let mut token = [0u8; 32];
                        if stream.read_exact(&mut token).await.is_ok() {
                            let _ = stream.write_all(&[1]).await;
                            let _ = stream.flush().await;
                        }
                    }
                });
            }
            Err(_) => return,
        }
    }
}

#[tokio::test]
async fn reregistration_on_remote_failure() {
    // Server-side Noise identity, shared by both mock tunnel endpoints.
    let key = Arc::new(
        Builder::new(NOISE_PARAMS.parse().unwrap())
            .generate_keypair()
            .unwrap(),
    );
    let public_key = hex::encode(&key.public);

    // Local server (simulates the application behind the tunnel)
    let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = local.local_addr().unwrap().port();
    tokio::spawn(accept_and_count(local, Arc::new(AtomicU32::new(0))));

    // Remote endpoint 1
    let remote1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote1_port = remote1.local_addr().unwrap().port();
    let remote1_count = Arc::new(AtomicU32::new(0));
    let remote1_task = tokio::spawn(noise_accept_and_count(
        remote1,
        remote1_count.clone(),
        key.clone(),
    ));

    // Remote endpoint 2 (ready before remote1 goes down)
    let remote2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote2_port = remote2.local_addr().unwrap().port();
    let remote2_count = Arc::new(AtomicU32::new(0));
    tokio::spawn(noise_accept_and_count(
        remote2,
        remote2_count.clone(),
        key.clone(),
    ));

    // Mock API server (returns whichever port endpoint_port holds)
    let endpoint_port = Arc::new(AtomicU16::new(remote1_port));
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(mock_api_server(api, endpoint_port.clone(), public_key));

    // Start the tunnel client with a zero re-registration window: the first
    // remote-connect failure then triggers re-registration immediately, which
    // cancels the per-connection reconnect backoff — so the test doesn't wait
    // for either the 30s default window or the 10s reconnect sleep. (The
    // windowing math itself is covered by the unit tests.)
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
    };
    open_tunnel(config).await.unwrap();

    // Phase 1: client connects to remote1
    tokio::time::timeout(Duration::from_secs(5), async {
        while remote1_count.load(Ordering::Relaxed) == 0 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("client should connect to remote1");

    // Kill remote1, switch API to return remote2's port
    endpoint_port.store(remote2_port, Ordering::Relaxed);
    remote1_task.abort();

    // Phase 2: client detects the failure, re-registers, connects to remote2.
    tokio::time::timeout(Duration::from_secs(10), async {
        while remote2_count.load(Ordering::Relaxed) == 0 {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("client should re-register and connect to remote2");

    let _ = shutdown_tx.send(());
}
