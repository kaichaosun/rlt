use std::sync::Arc;

use hyper::{Request, Response, body::Incoming, header::{UPGRADE, HOST}, upgrade::OnUpgrade, StatusCode};
use tokio::{net::TcpStream, sync::Mutex};

use crate::state::ClientManager;

/// Reverse proxy handler
pub async fn proxy_handler(mut req: Request<Incoming>, manager: Arc<Mutex<ClientManager>>) -> Result<Response<Incoming>, hyper::Error> {
    log::info!("uri ========= {}", req.uri());
    log::info!("host ========= {:?}", req.headers());
    let hostname = req.headers().get(HOST).unwrap().to_str().unwrap();
    log::info!("hostname ========= {}", hostname);

    let endpoint = extract(hostname.to_string());
    let mut manager = manager.lock().await;
    log::info!("endpoint: {}", endpoint);
    let client = manager.clients.get_mut(&endpoint).unwrap();
    let mut client = client.lock().await;
    let client_stream = client.take().await.unwrap();

    if !req.headers().contains_key(UPGRADE) {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await.unwrap();
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                log::error!("Connection failed: {:?}", err);
            }
        });

        sender.send_request(req).await
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await.unwrap();
            tokio::spawn(async move {
                if let Err(err) = conn.await {
                    log::error!("Connection failed: {:?}", err);
                }
            });

        let request_upgrade_type = req.headers().get(UPGRADE).unwrap().to_str().unwrap().to_string();
        let request_upgraded = req.extensions_mut().remove::<OnUpgrade>().unwrap();

        let mut response = sender.send_request(req).await.unwrap();

        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            let response_upgrade_type = response.headers().get(UPGRADE).unwrap().to_str().unwrap().to_string();
            if request_upgrade_type == response_upgrade_type {
                let mut response_upgraded = response.extensions_mut().remove::<OnUpgrade>()
                    .expect("response does not have an upgrade extension")
                    .await.unwrap();

                log::info!("Responding to a connection upgrade response");

                tokio::spawn(async move {
                    let mut request_upgraded = request_upgraded.await.expect("failed to upgrade request");
                    tokio::io::copy_bidirectional(&mut response_upgraded, &mut request_upgraded)
                        .await
                        .expect("coping between upgraded connections failed");
                });
            }
            Ok(response)
        } else {
            Ok(response)
        }
    }
}

fn extract(hostname: String) -> String {
    // TODO regex
    let hostname = hostname
        .replace("http://", "")
        .replace("https://", "")
        .replace("ws", "")
        .replace("wss", "");

    hostname.split(".").next().unwrap().to_string()
}
