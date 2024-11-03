use std::{collections::HashMap, io, sync::Arc, time::Duration};

use socket2::{SockRef, TcpKeepalive};
use tokio::{
    io::Interest,
    net::{TcpListener, TcpStream},
    sync::Mutex,
    time::timeout,
};

// See https://tldp.org/HOWTO/html_single/TCP-Keepalive-HOWTO to understand how keepalive work.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(not(target_os = "windows"))]
const TCP_KEEPALIVE_RETRIES: u32 = 5;

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
        let client = Arc::new(Mutex::new(Client::new(self.default_max_sockets)));
        self.clients.insert(url, client.clone());

        let mut client = client.lock().await;
        client.listen().await
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
                match timeout(Duration::from_secs(20), listener.accept()).await {
                    Ok(Ok((socket, addr))) => {
                        log::info!("new client connection: {:?}", addr);

                        let mut sockets = sockets.lock().await;
                        let sockets_len = sockets.len();

                        if sockets_len < max_sockets as usize {
                            log::debug!("Add a new socket {}/{max_sockets}", sockets_len + 1,);

                            let ka = TcpKeepalive::new()
                                .with_time(TCP_KEEPALIVE_TIME)
                                .with_interval(TCP_KEEPALIVE_INTERVAL);
                            #[cfg(not(target_os = "windows"))]
                            let ka = ka.with_retries(TCP_KEEPALIVE_RETRIES);
                            let sf = SockRef::from(&socket);
                            if let Err(err) = sf.set_tcp_keepalive(&ka) {
                                log::warn!("failed to enable TCP keepalive: {err}");
                            }

                            sockets.push(socket)
                        } else {
                            log::warn!("Reached sockets max: {sockets_len}/{max_sockets}");
                        }
                    }
                    Ok(Err(e)) => log::info!("Couldn't get client: {:?}", e),
                    Err(_) => {
                        // timeout clean up timeout connections
                        let mut sockets = sockets.lock().await;
                        let sockets_len = sockets.len();
                        let mut connected_sockets = vec![];
                        while let Some(s) = sockets.pop() {
                            if socket_is_writable(&s).await {
                                connected_sockets.push(s);
                            }
                        }

                        if sockets_len != connected_sockets.len() {
                            log::debug!(
                                "removed {} old disconnected sockets",
                                sockets_len - connected_sockets.len()
                            );
                        }
                        *sockets = connected_sockets;
                    }
                }
            }
        });

        Ok(port)
    }

    pub async fn take(&mut self) -> Option<TcpStream> {
        let mut sockets = self.available_sockets.lock().await;

        let sockets_len = sockets.len();
        let mut i = sockets_len;
        while let Some(socket) = sockets.pop() {
            log::debug!(
                "try using socket {i}/{sockets_len} (max: {})",
                self.max_sockets
            );

            if socket_is_writable(&socket).await {
                return Some(socket);
            }

            log::warn!(
                "socket {} is no longer writable, discard it",
                sockets.len() + 1
            );

            i -= 1;
        }
        None
    }
}

async fn socket_is_writable(socket: &TcpStream) -> bool {
    socket
        .ready(Interest::WRITABLE)
        .await
        // `is_write_closed` is set to `true` when keepalive times out
        .map(|ready| !ready.is_write_closed())
        .unwrap_or_default()
}
