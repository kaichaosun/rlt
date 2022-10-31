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
    log::info!("Request proxy endpoint, {}", endpoint);
    let mut manager = state.manager.lock().await;
    log::info!("get lock, {}", endpoint);
    manager.put(endpoint.to_string()).await.unwrap();

    let info = ProxyInfo {
        id: endpoint.to_string(),
        port: manager.clients.get(&endpoint.to_string()).unwrap().lock().await.port.unwrap(),
        max_conn_count: 10,
        url: format!("{}.localhost", endpoint.to_string()),

    };

    log::info!("proxy info, {:?}", info);
    HttpResponse::Ok().json(info)
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
