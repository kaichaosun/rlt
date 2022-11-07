use std::{collections::HashMap, sync::Arc, io::{self, ErrorKind}};

use tokio::{net::{TcpListener, TcpStream}, sync::Mutex};

/// App state holds all the client connection and status info.
pub struct State {
    pub manager: Arc<Mutex<ClientManager>>,
    pub max_sockets: u8,
    pub require_auth: bool,
}

pub struct ClientManager {
    pub clients: HashMap<String, Arc<Mutex<Client>>>,
    pub _tunnels: u16,
}

impl ClientManager {
    pub fn new() -> Self {
        ClientManager {
            clients: HashMap::new(),
            _tunnels: 0,
        }
    }

    pub async fn put(&mut self, url: String) -> io::Result<u16> {
        match self.clients.get(&url) {
            Some(client) => {
                client.lock().await.port.ok_or(io::Error::new(ErrorKind::Other, "Empty port"))
            },
            None => {
                let client = Arc::new(Mutex::new(Client::new()));
                self.clients.insert(url, client.clone() );
    
                let mut client = client.lock().await;
                client.listen().await
            }
        }
    }
}

pub struct Client {
    pub available_sockets: Arc<Mutex<Vec<TcpStream>>>,
    pub port: Option<u16>,
}

impl Client {
    pub fn new() -> Self {
        Client {
            available_sockets: Arc::new(Mutex::new(vec![])),
            port: None,
        }
    }
    pub async fn listen(&mut self) -> io::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        self.port = Some(port);

        let sockets = self.available_sockets.clone();

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, addr)) => {
                        log::info!("new client connection: {:?}", addr);
                        let mut sockets = sockets.lock().await;
                        sockets.push(socket)
                    },
                    Err(e) => log::info!("Couldn't get client: {:?}", e),
                }
            }
        });

        Ok(port)
    }

    pub async fn take(&mut self) -> Option<TcpStream> {
        let mut sockets = self.available_sockets.lock().await;
        sockets.pop()
    }
}
