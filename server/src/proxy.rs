use std::sync::Arc;

use anyhow::Result;
use hyper::{
    body::Incoming,
    header::{HOST, UPGRADE},
    upgrade::OnUpgrade,
    Request, Response, StatusCode,
};
use regex::Regex;
use tokio::sync::Mutex;

use crate::error::ServerError;
use crate::state::ClientManager;

/// Reverse proxy handler
pub async fn proxy_handler(
    mut req: Request<Incoming>,
    manager: Arc<Mutex<ClientManager>>,
) -> Result<Response<Incoming>> {
    let host_header = req.headers().get(HOST).ok_or(ServerError::NoHostHeader)?;
    let hostname = host_header.to_str()?;
    log::debug!("Request hostname: {}", hostname);

    let endpoint = extract(hostname)?;

    let client_stream = {
        let mut manager = manager.lock().await;
        let client = manager
            .clients
            .get_mut(&endpoint)
            .ok_or(ServerError::ProxyNotReady)?;
        let mut client = client.lock().await;
        client.take().await.ok_or(ServerError::EmptyConnection)?
    };
    let client_stream = hyper_util::rt::TokioIo::new(client_stream);

    if !req.headers().contains_key(UPGRADE) {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await?;
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                log::error!("Connection failed: {:?}", err);
            }
        });

        let response = sender.send_request(req).await?;
        Ok(response)
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await?;
        let conn = conn.with_upgrades();
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                log::error!("Connection failed: {:?}", err);
            }
        });

        let request_upgrade_type = req
            .headers()
            .get(UPGRADE)
            .ok_or(ServerError::NoUpgradeHeader)?
            .to_str()?
            .to_string();
        let request_upgraded = req
            .extensions_mut()
            .remove::<OnUpgrade>()
            .ok_or(ServerError::NoUpgradeExtension)?;

        let mut response = sender.send_request(req).await?;

        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            let response_upgrade_type = response
                .headers()
                .get(UPGRADE)
                .ok_or(ServerError::NoUpgradeHeader)?
                .to_str()?
                .to_string();
            if request_upgrade_type == response_upgrade_type {
                let response_upgraded = response
                    .extensions_mut()
                    .remove::<OnUpgrade>()
                    .ok_or(ServerError::NoUpgradeExtension)?
                    .await?;

                log::info!("Responding to a connection upgrade response");

                tokio::spawn(async move {
                    match request_upgraded.await {
                        Ok(request_upgraded) => {
                            let mut response_upgraded =
                                hyper_util::rt::TokioIo::new(response_upgraded);
                            let mut request_upgraded =
                                hyper_util::rt::TokioIo::new(request_upgraded);
                            if let Err(err) = tokio::io::copy_bidirectional(
                                &mut response_upgraded,
                                &mut request_upgraded,
                            )
                            .await
                            {
                                log::error!(
                                    "Coping between upgraded connections failed: {:?}",
                                    err
                                );
                            }
                        }
                        Err(err) => log::error!("Failed to upgrade request: {:?}", err),
                    }
                });
            }
            Ok(response)
        } else {
            Ok(response)
        }
    }
}

fn extract(hostname: &str) -> Result<String> {
    let re = Regex::new(r"(https?|wss?)://")?;
    let hostname = re.replace_all(hostname, "");

    let subdomain = hostname
        .split('.')
        .next()
        .ok_or(ServerError::InvalidHostName)?;

    Ok(subdomain.to_string())
}

#[cfg(test)]
mod tests {
    use super::extract;

    #[test]
    fn extract_subdomain_works() {
        let hostname = "demo.example.org";
        let subdomain = "demo".to_string();

        let domains = [
            &format!("http://{}", hostname),
            &format!("https://{}", hostname),
            &format!("ws://{}", hostname),
            &format!("wss://{}", hostname),
            hostname,
        ];

        for domain in domains {
            assert_eq!(extract(domain).unwrap(), subdomain);
        }
    }
}
