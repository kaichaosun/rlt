use std::{
    collections::HashMap,
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use snowstorm::{Builder, Keypair, NoiseStream};
use socket2::{SockRef, TcpKeepalive};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Interest},
    net::{TcpListener, TcpStream},
    sync::Mutex,
    task::JoinHandle,
    time::timeout,
};

// See https://tldp.org/HOWTO/html_single/TCP-Keepalive-HOWTO to understand how keepalive work.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(not(target_os = "windows"))]
const TCP_KEEPALIVE_RETRIES: u32 = 5;

/// How long before an unused client is cleaned up.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Noise protocol parameters for the encrypted tunnel between client and
/// server. Must match the client exactly; NK authenticates the server by its
/// static key (delivered to the client in the registration response) and
/// derives fresh session keys per connection.
pub const NOISE_PARAMS: &str = "Noise_NK_25519_ChaChaPoly_BLAKE2s";

/// Length of the per-tunnel session token the client must present after the
/// handshake before its connection joins the pool.
pub const SESSION_TOKEN_LEN: usize = 32;

/// How long a connecting peer gets to finish handshake + token before being
/// dropped, so half-open or non-speaking connections can't pile up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A pooled tunnel connection: encrypted and already authenticated by the
/// session token.
pub type TunnelStream = NoiseStream<TcpStream>;

/// App state holds all the client connection and status info.
pub struct State {
    pub manager: Arc<Mutex<ClientManager>>,
    pub max_sockets: u8,
    pub require_auth: bool,
    pub secure: bool,
    pub domain: String,
    /// Hex-encoded static Noise public key, handed to clients at registration.
    pub public_key: String,
}

pub struct ClientManager {
    pub clients: HashMap<String, Arc<Mutex<Client>>>,
    pub _tunnels: u16,
    pub default_max_sockets: u8,
    key: Arc<Keypair>,
}

impl ClientManager {
    pub fn new(max_sockets: u8, key: Arc<Keypair>) -> Self {
        ClientManager {
            clients: HashMap::new(),
            _tunnels: 0,
            default_max_sockets: max_sockets,
            key,
        }
    }

    /// Registers a tunnel and returns the assigned port together with the
    /// hex-encoded session token the client must present on every connection.
    pub async fn put(&mut self, url: String) -> io::Result<(u16, String)> {
        let session_token: [u8; SESSION_TOKEN_LEN] = rand::random();
        let client = Arc::new(Mutex::new(Client::new(
            self.default_max_sockets,
            self.key.clone(),
            session_token,
        )));
        self.clients.insert(url, client.clone());

        let mut client = client.lock().await;
        let port = client.listen().await?;
        Ok((port, hex::encode(session_token)))
    }

    /// clean up old unused clients
    pub async fn cleanup(&mut self) {
        let mut to_remove = vec![];

        for (url, client) in self.clients.iter() {
            let client = client.lock().await;
            if client.should_cleanup().await {
                to_remove.push(url.clone());
            }
        }

        for url in to_remove {
            log::debug!("cleanup client {url}");
            self.clients.remove(url.as_str());
        }
    }
}

pub struct Client {
    pub available_sockets: Arc<Mutex<Vec<TunnelStream>>>,
    pub port: Option<u16>,
    pub max_sockets: u8,
    key: Arc<Keypair>,
    session_token: [u8; SESSION_TOKEN_LEN],
    listen_task: Option<JoinHandle<()>>,
    /// last time a new connection was established
    last_connection_time: Instant,
}

impl Client {
    pub fn new(max_sockets: u8, key: Arc<Keypair>, session_token: [u8; SESSION_TOKEN_LEN]) -> Self {
        Client {
            available_sockets: Arc::new(Mutex::new(vec![])),
            port: None,
            max_sockets,
            key,
            session_token,
            listen_task: None,
            last_connection_time: std::time::Instant::now(),
        }
    }

    pub async fn listen(&mut self) -> io::Result<u16> {
        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let port = listener.local_addr()?.port();
        self.port = Some(port);

        let sockets = self.available_sockets.clone();
        let max_sockets = self.max_sockets;
        let key = self.key.clone();
        let session_token = self.session_token;

        let listen_task = tokio::spawn(async move {
            loop {
                match timeout(Duration::from_secs(20), listener.accept()).await {
                    Ok(Ok((socket, addr))) => {
                        log::info!("new client connection: {:?}", addr);

                        let sockets = sockets.clone();
                        let key = key.clone();

                        // Handshake in its own task so a slow or hostile peer
                        // can't stall the accept loop.
                        tokio::spawn(async move {
                            if sockets.lock().await.len() >= max_sockets as usize {
                                log::warn!("Reached sockets max: {max_sockets}, dropping connection");
                                return;
                            }

                            let ka = TcpKeepalive::new()
                                .with_time(TCP_KEEPALIVE_TIME)
                                .with_interval(TCP_KEEPALIVE_INTERVAL);
                            #[cfg(not(target_os = "windows"))]
                            let ka = ka.with_retries(TCP_KEEPALIVE_RETRIES);
                            let sf = SockRef::from(&socket);
                            if let Err(err) = sf.set_tcp_keepalive(&ka) {
                                log::warn!("failed to enable TCP keepalive: {err}");
                            }

                            let stream = match timeout(
                                HANDSHAKE_TIMEOUT,
                                secure_accept(socket, &key, &session_token),
                            )
                            .await
                            {
                                Ok(Ok(stream)) => stream,
                                Ok(Err(err)) => {
                                    log::warn!("rejected tunnel connection from {addr:?}: {err:?}");
                                    return;
                                }
                                Err(_) => {
                                    log::warn!("tunnel handshake with {addr:?} timed out");
                                    return;
                                }
                            };

                            let mut sockets = sockets.lock().await;
                            let sockets_len = sockets.len();
                            if sockets_len < max_sockets as usize {
                                log::debug!("Add a new socket {}/{max_sockets}", sockets_len + 1);
                                sockets.push(stream);
                            } else {
                                log::warn!("Reached sockets max: {sockets_len}/{max_sockets}");
                            }
                        });
                    }
                    Ok(Err(e)) => log::info!("Couldn't get client: {:?}", e),
                    Err(_) => {
                        // timeout clean up timeout connections
                        let mut sockets = sockets.lock().await;
                        let sockets_len = sockets.len();
                        let mut connected_sockets = vec![];
                        while let Some(s) = sockets.pop() {
                            if socket_is_writable(s.get_inner()).await {
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
        self.listen_task = Some(listen_task);

        Ok(port)
    }

    pub async fn take(&mut self) -> Option<TunnelStream> {
        self.last_connection_time = Instant::now();
        let mut sockets = self.available_sockets.lock().await;

        let sockets_len = sockets.len();
        let mut i = sockets_len;
        while let Some(socket) = sockets.pop() {
            log::debug!(
                "try using socket {i}/{sockets_len} (max: {})",
                self.max_sockets
            );

            if socket_is_writable(socket.get_inner()).await {
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

    /// If the client has not been used for a while and so should be cleaned up.
    pub async fn should_cleanup(&self) -> bool {
        let sockets = self.available_sockets.lock().await;

        sockets.is_empty() && self.last_connection_time.elapsed() > CLEANUP_TIMEOUT
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(task) = self.listen_task.take() {
            task.abort();
        }
    }
}

/// Complete the Noise handshake as responder, then require the tunnel's
/// session token as the first encrypted message. Only authenticated
/// connections may join the pool — this is what stops an arbitrary peer that
/// found the port from receiving proxied traffic. A one-byte ack is sent back
/// so the client can distinguish "accepted" from "rejected" instead of
/// discovering it later through a dead proxied request.
async fn secure_accept(
    socket: TcpStream,
    key: &Keypair,
    session_token: &[u8; SESSION_TOKEN_LEN],
) -> Result<TunnelStream> {
    let responder = Builder::new(NOISE_PARAMS.parse()?)
        .local_private_key(&key.private)
        .build_responder()?;
    let mut stream = NoiseStream::handshake(socket, responder)
        .await
        .map_err(|err| anyhow!("noise handshake failed: {err:?}"))?;

    let mut received = [0u8; SESSION_TOKEN_LEN];
    stream.read_exact(&mut received).await?;
    if !token_matches(&received, session_token) {
        return Err(anyhow!("session token mismatch"));
    }

    stream.write_all(&[1]).await?;
    stream.flush().await?;

    Ok(stream)
}

fn token_matches(a: &[u8; SESSION_TOKEN_LEN], b: &[u8; SESSION_TOKEN_LEN]) -> bool {
    // Constant-time comparison, no dependence on where the first mismatch is.
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn socket_is_writable(socket: &TcpStream) -> bool {
    socket
        .ready(Interest::WRITABLE)
        .await
        // `is_write_closed` is set to `true` when keepalive times out
        .map(|ready| !ready.is_write_closed())
        .unwrap_or_default()
}
