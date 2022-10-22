use std::{collections::HashMap, net::SocketAddr, io, cell::Cell, sync::{Arc, Mutex}};

use actix_web::{get, web, App, HttpServer, Responder, HttpResponse};
use serde::{Serialize, Deserialize};
use tokio::net::{TcpListener, TcpStream};

struct State {
    manager: Mutex<ClientManager>,
}

#[get("/hello/{name}")]
async fn greet(name: web::Path<String>) -> impl Responder {
    format!("Hello {name}!")
}

#[get("/api/status")]
async fn status() -> impl Responder {
    let status = ApiStatus {
        tunnels_count: 10,
        tunels: "kaichao".to_string(),
    };

    HttpResponse::Ok().json(status)
}

#[get("/{endpoint}")]
async fn proxy(endpoint: web::Path<String>, state: web::Data<State>) -> impl Responder {
    let mut manager = state.manager.lock().unwrap();
    manager.put(endpoint.to_string()).await.unwrap();


    format!("{endpoint}!")
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiStatus {
    tunnels_count: u16,
    tunels: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProxyInfo {
    url: String,
}

struct ClientManager {
    clients: HashMap<String, Client>,
    tunnels: u16,
}

impl ClientManager {
    pub fn new() -> Self {
        ClientManager {
            clients: HashMap::new(),
            tunnels: 0,
        }
    }

    pub async fn put(&mut self, url: String) -> io::Result<()> {
        if self.clients.get(&url).is_none() {
            let mut client = Client::new();
            client.listen().await?;   
            self.clients.insert(url, client );
        }

        Ok(())
    }
}

struct Client {
    available_sockets: Vec<TcpStream>,
}

impl Client {
    pub fn new() -> Self {
        Client {
            available_sockets: vec![],
        }
    }
    pub async fn listen(&mut self) -> io::Result<()> {
        // TODO port should > 1000
        let listener = TcpListener::bind("127.0.0.1:0").await?;

        match listener.accept().await {
            Ok((socket, addr)) => {
                println!("new client connection: {:?}", addr);
                self.available_sockets.push(socket)
            },
            Err(e) => println!("Couldn't get client: {:?}", e),
        }

        Ok(())
    }
}

pub async fn create(domain: String, port: u16, secure: bool, max_sockets: u8) {
    log::info!("Create proxy server at {} {} {} {}", &domain, port, secure,  max_sockets);

    let state = web::Data::new(State {
        manager: Mutex::new(ClientManager::new()),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .service(greet)
            .service(status)
            .service(proxy)
    })
    .bind(("127.0.0.1", 8080)).unwrap()
    .run()
    .await
    .unwrap()
}

// create the endpoint, proxy.threethain.dev/did-123, proxy.threethain.xyz?new
// create a new clent manager, the manager should listen on the assigned port
// send request to the custom domain, get client id
// get the client manager with client id
// client manager handle the request.
