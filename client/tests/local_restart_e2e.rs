use localtunnel_client::{broadcast, open_tunnel, ClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration};

/// A minimal HTTP/1.1 server answering `200 OK` with `tag` as the body, so the
/// test can tell the original process apart from the restarted one. The returned
/// handle is what makes the restart observable: aborting it stops the accept loop
/// the way killing the tunneled service would.
async fn local_http(port: u16, tag: &'static str) -> JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                loop {
                    match stream.read(&mut buffer).await {
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

/// Issues a request through the public side of the tunnel and returns the status
/// line and the body joined together, so a single `contains` covers both. Errors
/// are folded into the string rather than panicking: while the local service is
/// down every outcome here is legitimate, and the caller decides what to assert.
async fn proxy_get(proxy_port: u16, host: &str) -> String {
    let mut stream = match TcpStream::connect(("127.0.0.1", proxy_port)).await {
        Ok(stream) => stream,
        Err(err) => return format!("CONNECT_ERR {err}"),
    };
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return "WRITE_ERR".to_string();
    }

    let mut raw = String::new();
    let _ = timeout(Duration::from_secs(5), stream.read_to_string(&mut raw)).await;
    let status_line = raw.lines().next().unwrap_or("EMPTY").to_string();
    let body = raw.rsplit("\r\n\r\n").next().unwrap_or("");
    format!("{status_line} | {body}")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// End-to-end non-regression test for the scenario issue #8 is named after, run
// against a real localtunnel-server rather than a mock: restart the tunneled
// service and the public URL must serve it again.
//
// This test is green on f86f32b too — the simple case already worked there. It
// exists because phases 1-3 rewrote the whole connection path (lazy local
// connect, half-close handling, reconnect backoff) and none of that is allowed
// to break the one scenario the issue actually reports. It runs on the shipped
// defaults on purpose: `reconnect_base_delay` and `reconnect_max_delay` are left
// `None` so the measured recovery is the one a real user gets.
//
// The server runs on its own thread with its own runtime because
// `actix_web::HttpServer` is not `Send`, so `tokio::spawn` will not take it.
#[tokio::test(flavor = "multi_thread")]
async fn local_service_restart_is_served_again() {
    let api_port = free_port();
    let proxy_port = free_port();
    let local_port = free_port();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let _ = localtunnel_server::start(localtunnel_server::ServerConfig {
                domain: format!("127.0.0.1:{proxy_port}"),
                api_port,
                secure: false,
                max_sockets: 10,
                proxy_port,
                require_auth: false,
            })
            .await;
        });
    });

    timeout(Duration::from_secs(10), async {
        while TcpStream::connect(("127.0.0.1", api_port)).await.is_err() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("localtunnel-server should come up");

    let first = local_http(local_port, "V1").await;

    let (shutdown_tx, _) = broadcast::channel(1);
    let url = open_tunnel(ClientConfig {
        server: Some(format!("http://127.0.0.1:{api_port}")),
        subdomain: Some("test".to_string()),
        local_host: Some("127.0.0.1".to_string()),
        local_port,
        shutdown_signal: shutdown_tx.clone(),
        max_conn: 10,
        credential: None,
        reregister_after: None,
        reconnect_base_delay: None,
        reconnect_max_delay: None,
    })
    .await
    .unwrap();
    println!("tunnel url = {url}");
    sleep(Duration::from_secs(1)).await;

    let host = format!("test.127.0.0.1:{proxy_port}");
    let before = proxy_get(proxy_port, &host).await;
    assert!(
        before.contains("200 OK") && before.contains("V1"),
        "the tunnel must serve the local service before the restart, got: {before}"
    );

    // Kill the tunneled service. What the tunnel answers while it is down is not
    // asserted here — phase 1 covers the 502 — because pinning it down would make
    // this test brittle for no extra coverage.
    first.abort();
    sleep(Duration::from_millis(300)).await;
    println!("while down: {}", proxy_get(proxy_port, &host).await);

    // Restart it on the same port, as a service restart would.
    let restarted = local_http(local_port, "V2").await;
    let recovered = timeout(Duration::from_secs(10), async {
        loop {
            let response = proxy_get(proxy_port, &host).await;
            if response.contains("200 OK") && response.contains("V2") {
                return response;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the tunnel must serve the restarted local service again");
    assert!(recovered.contains("V2"), "unexpected body: {recovered}");

    let _ = shutdown_tx.send(());
    drop(restarted);
}
