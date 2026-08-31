use std::sync::Arc;

use anyhow::Result;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    header::{CONTENT_TYPE, HOST, UPGRADE},
    upgrade::OnUpgrade,
    Request, Response, StatusCode,
};
use regex::Regex;
use tokio::sync::Mutex;

use crate::error::ServerError;
use crate::state::ClientManager;

/// Either a response streamed from the tunnel client, or one generated here.
type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Builds a plain-text response describing a proxying failure.
///
/// Answering with a real status matters: returning an error from the service
/// instead makes hyper drop the connection without a reply, which any reverse
/// proxy in front reports as a generic upstream failure and which shows up here
/// only as an opaque `hyper::Error`.
fn error_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(format!("{message}\n")))
        .map_err(|never| match never {})
        .boxed();

    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body)
        .expect("error response is always well-formed")
}

/// Reverse proxy handler
pub async fn proxy_handler(
    mut req: Request<Incoming>,
    manager: Arc<Mutex<ClientManager>>,
) -> Result<Response<ProxyBody>> {
    let Some(host_header) = req.headers().get(HOST) else {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "Request must contain a Host header",
        ));
    };
    let Ok(hostname) = host_header.to_str() else {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "Host header is not valid text",
        ));
    };
    log::debug!("Request hostname: {}", hostname);

    let endpoint = extract(hostname)?;

    let client_stream = {
        let mut manager = manager.lock().await;
        let Some(client) = manager.clients.get_mut(&endpoint) else {
            log::warn!("No tunnel registered for endpoint {}", endpoint);
            return Ok(error_response(
                StatusCode::NOT_FOUND,
                "No tunnel is registered for this hostname",
            ));
        };
        let mut client = client.lock().await;
        match client.take().await {
            Some(stream) => stream,
            None => {
                log::warn!("No connection available for endpoint {}", endpoint);
                return Ok(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The tunnel for this hostname has no connection available",
                ));
            }
        }
    };
    let client_stream = hyper_util::rt::TokioIo::new(client_stream);

    if !req.headers().contains_key(UPGRADE) {
        let (mut sender, conn) = match hyper::client::conn::http1::handshake(client_stream).await {
            Ok(pair) => pair,
            Err(err) => {
                log::error!("Handshake with the tunnel client failed: {:?}", err);
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    "Could not talk to the tunnel client",
                ));
            }
        };
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                log::error!("Connection failed: {:?}", err);
            }
        });

        match sender.send_request(req).await {
            Ok(response) => Ok(response.map(BodyExt::boxed)),
            Err(err) => {
                log::error!("Request to the tunnel client failed: {:?}", err);
                Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    "The tunnel client did not answer the request",
                ))
            }
        }
    } else {
        let (mut sender, conn) = match hyper::client::conn::http1::handshake(client_stream).await {
            Ok(pair) => pair,
            Err(err) => {
                log::error!("Handshake with the tunnel client failed: {:?}", err);
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    "Could not talk to the tunnel client",
                ));
            }
        };
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

        let mut response = match sender.send_request(req).await {
            Ok(response) => response,
            Err(err) => {
                log::error!("Upgrade request to the tunnel client failed: {:?}", err);
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    "The tunnel client did not answer the upgrade request",
                ));
            }
        };

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
            Ok(response.map(BodyExt::boxed))
        } else {
            Ok(response.map(BodyExt::boxed))
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
