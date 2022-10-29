// create the endpoint, proxy.threethain.dev/did-123, proxy.threethain.xyz?new
// create a new clent manager, the manager should listen on the assigned port
// send request to the custom domain, get client id
// get the client manager with client id
// client manager handle the request.

use std::{collections::HashMap, sync::{Mutex, Arc}, net::SocketAddr, io};

use actix_web::{get, web, App, HttpServer, Responder, HttpResponse, dev::ConnectionInfo};
use hyper::{service::service_fn, server::conn::http1, header::{UPGRADE, HOST}, Response, Request, upgrade::Upgraded};
use serde::{Serialize, Deserialize};
use tokio::{net::{TcpListener, TcpStream}};

struct State {
    manager: Arc<Mutex<ClientManager>>,
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
async fn request_endpoint(endpoint: web::Path<String>, state: web::Data<State>) -> impl Responder {
    log::info!("Request proxy endpoint, {}", endpoint);
    let mut manager = state.manager.lock().unwrap();
    log::info!("get lock, {}", endpoint);
    manager.put(endpoint.to_string()).await.unwrap();

    let info = ProxyInfo {
        id: endpoint.to_string(),
        port: manager.clients.get(&endpoint.to_string()).unwrap().lock().unwrap().port.unwrap(),
        max_conn_count: 10,
        url: format!("{}.localhost", endpoint.to_string()),

    };

    log::info!("proxy info, {:?}", info);
    HttpResponse::Ok().json(info)
}

// TODO use tokio tcplistener directly, no need for authentiacation, since it's from public user requests
#[get("/")]
async fn request(conn: ConnectionInfo) -> impl Responder {
    let host = conn.host();

    format!("hello {host}")
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

struct ClientManager {
    clients: HashMap<String, Arc<Mutex<Client>>>,
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
            let client = Arc::new(Mutex::new(Client::new()));
        
            self.clients.insert(url, client.clone() );

            let mut client = client.lock().unwrap();
            client.listen().await.unwrap();
            
        }

        Ok(())
    }
}

struct Client {
    available_sockets: Arc<Mutex<Vec<TcpStream>>>,
    port: Option<u16>,
}

impl Client {
    pub fn new() -> Self {
        Client {
            available_sockets: Arc::new(Mutex::new(vec![])),
            port: None,
        }
    }
    pub async fn listen(&mut self) -> io::Result<()> {
        // TODO port should > 1000 65535 range
        // https://github.com/rust-lang-nursery/rust-cookbook/issues/500
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr().unwrap().port();
        self.port = Some(port);

        let sockets = self.available_sockets.clone();

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, addr)) => {
                        println!("new client connection: {:?}", addr);
                        let mut sockets = sockets.lock().unwrap();
                        sockets.push(socket)
                    },
                    Err(e) => println!("Couldn't get client: {:?}", e),
                }
            }
        });

        Ok(())
    }

    pub fn take(&mut self) -> Option<TcpStream> {
        let mut sockets = self.available_sockets.lock().unwrap();
        sockets.pop()
    }
}


async fn service_handler(req: Request<hyper::body::Incoming>, manager: Arc<Mutex<ClientManager>>) -> Result<Response<hyper::body::Incoming>, hyper::Error> {
    println!("uri ========= {}", req.uri());
    println!("host ========= {:?}", req.headers());
    let hostname = req.headers().get(HOST).unwrap().to_str().unwrap();
    println!("hostname ========= {}", hostname);

    let endpoint = extract(hostname.to_string());
    let mut manager = manager.lock().unwrap();
    log::info!("endpoint: {}", endpoint);
    let client = manager.clients.get_mut(&endpoint).unwrap();
    let mut client = client.lock().unwrap();
    let mut client_stream = client.take().unwrap();

    if !req.headers().contains_key(UPGRADE) {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await.unwrap();
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                log::error!("Connection failed: {:?}", err);
            }
        });

        sender.send_request(req).await
    } else {
        // let req = Arc::new(Mutex::new(req));
        // tokio::spawn(async move {
        //     match hyper::upgrade::on(req).await {
        //         Ok(mut upgraded) => {
        //             tokio::io::copy_bidirectional(&mut upgraded, &mut client_stream).await.unwrap();
        //         },
        //         Err(e) => log::error!("Error on upgrade, {:?}", e),
        //     }
        // });

        // let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await.unwrap();
        //     tokio::spawn(async move {
        //         if let Err(err) = conn.await {
        //             log::error!("Connection failed: {:?}", err);
        //         }
        //     });

        // // let req = req.clone().lock().unwrap();
        // sender.send_request(req).await

        let (response, websocket) = hyper_tungstenite::upgrade(request, None).unwrap();
        
        Ok(response)
    }
}

// TODO proxy_port, port -> admin_port
// require_auth: bool
// start a tcplistener on proxy port
pub async fn create(domain: String, port: u16, secure: bool, max_sockets: u8) {
    log::info!("Create proxy server at {} {} {} {}", &domain, port, secure,  max_sockets);

    let manager = Arc::new(Mutex::new(ClientManager::new()));
    let state = web::Data::new(State {
        manager: manager.clone(),
    });

    tokio::spawn(async move {
        let addr: SocketAddr = ([127, 0, 0, 1], 3001).into();
        log::info!("listening on {}", addr);
        let listener = TcpListener::bind(addr).await.unwrap();

        loop {
            let (stream, _) = listener.accept().await.unwrap();

            log::info!("Accept proxy request");

            // This is the `Service` that will handle the connection.
            // `service_fn` is a helper to convert a function that
            // returns a Response into a `Serive`.
            let manager = manager.clone();
            // let service = service_fn(move |req| {
            //     println!("uri ========= {}", req.uri());
            //     println!("host ========= {:?}", req.headers());
            //     let hostname = req.headers().get(HOST).unwrap().to_str().unwrap();
            //     println!("hostname ========= {}", hostname);

            //     let endpoint = extract(hostname.to_string());
            //     let mut manager = manager.lock().unwrap();
            //     log::info!("endpoint: {}", endpoint);
            //     let client = manager.clients.get_mut(&endpoint).unwrap();
            //     let mut client = client.lock().unwrap();
            //     let client_stream = client.take().unwrap();

            //     async move {
            //         let (mut sender, conn) = hyper::client::conn::http1::handshake(client_stream).await.unwrap();
            //         tokio::spawn(async move {
            //             if let Err(err) = conn.await {
            //                 log::error!("Connection failed: {:?}", err);
            //             }
            //         });

            //         sender.send_request(req).await
            //     }
            // });

            tokio::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(stream, service_fn(move |req| service_handler(req, manager.clone())))
                    .await
                {
                    log::error!("Failed to serve connection: {:?}", err);
                }
            });
        }
    });

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .service(greet)
            .service(status)
            .service(request_endpoint)
            .service(request)
    })
    .bind(("127.0.0.1", port)).unwrap()
    .run()
    .await
    .unwrap();
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

// async fn _proxy(req: Request<Body>) -> Result<Response, hyper::Error> {
//     log::info!("Request: {:?}", req);

//     if let Some(host_addr) = req.uri().authority().map(|auth| auth.to_string()) {
//         tokio::task::spawn(async move {
//             match hyper::upgrade::on(req).await {
//                 Ok(upgraded) => {
//                     if let Err(e) = _tunnel(upgraded, host_addr).await {
//                         log::warn!("server io error: {}", e);
//                     };
//                 }
//                 Err(e) => log::warn!("upgrade error: {}", e),
//             }
//         });

//         Ok(Response::new(body::boxed(body::Empty::new())))
//     } else {
//         log::warn!("CONNECT host is not socket addr: {:?}", req.uri());
//         Ok((
//             StatusCode::BAD_REQUEST,
//             "CONNECT must be to a socket address",
//         )
//             .into_response())
//     }
// }

async fn tunnel(mut upgraded: Upgraded, client_stream: &mut TcpStream) -> std::io::Result<()> {
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, client_stream).await?;

    log::info!(
        "client wrote {} bytes and received {} bytes",
        from_client,
        from_server
    );

    Ok(())
}
