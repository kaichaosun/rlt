use std::{collections::HashMap, sync::Arc, io::{self, ErrorKind}};

use tokio::{net::{TcpListener, TcpStream}, sync::Mutex};

/// App state holds all the client connection and status info.
pub struct State {
    pub manager: Arc<Mutex<ClientManager>>,
    pub max_sockets: u8,
    pub require_auth: bool,
    pub secure: bool,
    pub domain: String,
}

pub struct ClientManager {
    pub clients: HashMap<String, Arc<Mutex<Client>>>,
    pub _tunnels: u16,
    pub default_max_sockets: u8,
}

impl ClientManager {
    pub fn new(max_sockets: u8) -> Self {
        ClientManager {
            clients: HashMap::new(),
            _tunnels: 0,
            default_max_sockets: max_sockets,
        }
    }

    pub async fn put(&mut self, url: String) -> io::Result<u16> {
        match self.clients.get(&url) {
            Some(client) => {
                client.lock().await.port.ok_or(io::Error::new(ErrorKind::Other, "Empty port"))
            },
            None => {
                let client = Arc::new(Mutex::new(Client::new(self.default_max_sockets)));
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
    pub max_sockets: u8,
}

impl Client {
    pub fn new(max_sockets: u8) -> Self {
        Client {
            available_sockets: Arc::new(Mutex::new(vec![])),
            port: None,
            max_sockets,
        }
    }

    pub async fn listen(&mut self) -> io::Result<u16> {
        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let port = listener.local_addr()?.port();
        self.port = Some(port);

        let sockets = self.available_sockets.clone();
        let max_sockets = self.max_sockets;

        tokio::spawn(async move {
            // TODO check client is authenticated for the port
            loop {
                match listener.accept().await {
                    Ok((socket, addr)) => {
                        log::info!("new client connection: {:?}", addr);
                        
                        let mut sockets = sockets.lock().await;
                        log::debug!("Sockets length: {}", sockets.len());
                        if sockets.len() < max_sockets as usize {
                            log::debug!("Add a new socket, max: {}", max_sockets);
                            sockets.push(socket)
                        }
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
