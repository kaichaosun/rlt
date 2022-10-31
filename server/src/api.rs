use actix_web::{get, web, Responder, HttpResponse};
use serde::{Serialize, Deserialize};

use crate::state::State;

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
/// TODO add validation to the endpoint, and check query new.
#[get("/{endpoint}")]
pub async fn request_endpoint(endpoint: web::Path<String>, state: web::Data<State>) -> impl Responder {
    log::debug!("Request proxy endpoint, {}", endpoint);

    let mut manager = state.manager.lock().await;
    match manager.put(endpoint.to_string()).await {
        Ok(port) => {
            let info = ProxyInfo {
                id: endpoint.to_string(),
                port,
                max_conn_count: 10, // TODO use from server param passed in
                url: format!("{}.localhost", endpoint.to_string()),
        
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
