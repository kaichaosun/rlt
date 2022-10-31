use std::{collections::HashMap, sync::{Mutex, Arc}, io};

use tokio::{net::{TcpListener, TcpStream}};

/// App state holds all the client connection and status info.
pub struct State {
    pub manager: Arc<Mutex<ClientManager>>,
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
    pub async fn listen(&mut self) -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr().unwrap().port();
        self.port = Some(port);

        let sockets = self.available_sockets.clone();

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, addr)) => {
                        log::info!("new client connection: {:?}", addr);
                        let mut sockets = sockets.lock().unwrap();
                        sockets.push(socket)
                    },
                    Err(e) => log::info!("Couldn't get client: {:?}", e),
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
