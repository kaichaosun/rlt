use actix_web::{get, web, Responder, HttpResponse};
use serde::{Serialize, Deserialize};
use regex::Regex;
use anyhow::Result;

use crate::state::State;
use crate::auth::{Auth, CfWorkerStore};

/// TODO get tunnel status from state
#[get("/api/status")]
pub async fn api_status() -> impl Responder {
    let status = ApiStatus {
        tunnels_count: 0,
        tunels: "kaichao".to_string(),
    };

    HttpResponse::Ok().json(status)
}

/// Request proxy endpoint
#[get("/{endpoint}")]
pub async fn request_endpoint(endpoint: web::Path<String>, info: web::Query<AuthInfo>, state: web::Data<State>) -> impl Responder {
    log::debug!("Request proxy endpoint, {}", endpoint);
    log::debug!("Require auth: {}", state.require_auth);

    match validate_endpoint(&endpoint) {
        Ok(true) => (),
        Ok(false) => return HttpResponse::BadRequest().body("Request subdomain is invalid, only chars in lowercase and numbers are allowed"),
        Err(err) => return HttpResponse::InternalServerError().body(format!("Server Error: {:?}", err)),
    }

    if state.require_auth {
        let credential = match info.credential.clone() {
            Some(val) => val,
            None => return HttpResponse::BadRequest().body("Request Error: credential param is empty.")
        };

        match CfWorkerStore.credential_is_valid(&credential, &endpoint).await {
            Ok(true) => (),
            Ok(false) => return HttpResponse::BadRequest().body(format!("Error: credential is not valid.")),
            Err(err) => {
                log::error!("Server error: {:?}", err);
                return HttpResponse::InternalServerError().body(format!("Server Error: {:?}", err))
            }
        };
    }

    let mut manager = state.manager.lock().await;
    match manager.put(endpoint.to_string()).await {
        Ok(port) => {
            let schema = if state.secure { "https" } else {"http"};
            let info = ProxyInfo {
                id: endpoint.to_string(),
                port,
                max_conn_count: state.max_sockets,
                url: format!("{}://{}.{}", schema, endpoint.to_string(), state.domain),
            };
        
            log::debug!("Proxy info, {:?}", info);
            HttpResponse::Ok().json(info)
        },
        Err(e) => {
            log::error!("Client manager failed to put proxy endpoint: {:?}", e);
            return HttpResponse::InternalServerError().body(format!("Error: {:?}", e))
        }
    }
}

fn validate_endpoint(endpoint: &str) -> Result<bool> {
    // Don't allow A-Z uppercase since it will convert to lowercase in browser
    let re = Regex::new("^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")?;
    Ok(re.is_match(endpoint))
}

#[derive(Debug, Deserialize)]
pub struct AuthInfo {
    credential: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiStatus {
    tunnels_count: u16,
    tunels: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProxyInfo {
    id: String,
    port: u16,
    max_conn_count: u8,
    url: String,
}

#[cfg(test)]
mod tests {
    use crate::api::validate_endpoint;

    #[test]
    fn validate_endpoint_works() {
        let endpoints = [
            "demo",
            "123",
            "did-key-zq3shkkuzlvqefghdgzgfmux8vgkgvwsla83w2oekhzxocw2n",
        ];

        for endpoint in endpoints {
            assert_eq!(validate_endpoint(endpoint).unwrap(), true);
        }
    }
}