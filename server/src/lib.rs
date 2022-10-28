use std::{collections::HashMap, io::{ErrorKind}, sync::{Mutex, Arc}, pin::Pin, task::{Context, Poll}, cmp::min};

use actix_web::{get, web, App, HttpServer, Responder, HttpResponse, dev::ConnectionInfo};
use byteorder::{NetworkEndian, ByteOrder};
use serde::{Serialize, Deserialize};
use tokio::{net::{TcpListener, TcpStream}, io::{self, AsyncRead, ReadBuf, Error, AsyncReadExt}};
use tokio::pin;
use tldextract::{TldExtractor, TldOption};
use pin_project::pin_project;

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
async fn proxy(endpoint: web::Path<String>, state: web::Data<State>) -> impl Responder {
    let mut manager = state.manager.lock().unwrap();
    manager.put(endpoint.to_string()).await.unwrap();


    format!("{endpoint}!")
}

// TODO use tokio tcplistener directly, no need for authentiacation, since it's from public user requests
#[get("/")]
async fn request(conn: ConnectionInfo, state: web::Data<State>) -> impl Responder {
    let host = conn.host();

    let tld: TldExtractor = TldOption::default().build();
    if let Ok(uri) = tld.extract(host) {
        if let Some(endpoint) = uri.subdomain {
            let manager = state.manager.lock().unwrap();
            let client = manager.clients.get(&endpoint).unwrap();
            let socket = &client.available_sockets[0];
        }
    }
    format!("hello {host}")
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
        let proxy_port = 3001; // TODO use passed in port
        let proxy_listen_addr = format!("127.0.0.1:{proxy_port}");
        log::info!("Listening proxy request on: {}", proxy_listen_addr);
    
        let listener = TcpListener::bind(proxy_listen_addr).await.unwrap();
    
        loop {
            let (mut client_stream, client_addr) = listener.accept().await.unwrap();
    
            let mut recording_reader = RecordingBufReader::new(&mut client_stream);
            let reader = HandshakeRecordReader::new(&mut recording_reader);
            pin!(reader);
    
            let hostname = read_sni_host_name_from_client_hello(reader).await;
            match hostname {
                Ok(hostname) => {
                    let tld: TldExtractor = TldOption::default().build();
                    if let Ok(uri) = tld.extract(&hostname) {
                        if let Some(endpoint) = uri.subdomain {
                            log::info!("Proxy endpoint: {}", endpoint);
                            let manager = manager.lock().unwrap();
                            let client = manager.clients.get(&endpoint).unwrap();
                            let socket = &client.available_sockets[0];
                        }
                    }
                },
                Err(err) => {
                    log::error!("Error happens: {}", err);
                }
            }
            
        }
    });

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .service(greet)
            .service(status)
            .service(proxy)
            .service(request)
    })
    .bind(("127.0.0.1", port)).unwrap()
    .run()
    .await
    .unwrap();
}

#[pin_project]
struct RecordingBufReader<R: AsyncRead> {
    #[pin]
    reader: R,
    buf: Vec<u8>,
    read_offset: usize,
}

const RECORDING_READER_BUF_SIZE: usize = 1 << 10; // 1 KiB

impl<R: AsyncRead> RecordingBufReader<R> {
    fn new(reader: R) -> RecordingBufReader<R> {
        RecordingBufReader {
            reader,
            buf: Vec::with_capacity(RECORDING_READER_BUF_SIZE),
            read_offset: 0,
        }
    }

    fn buf(self) -> Vec<u8> {
        self.buf
    }
}

impl<R: AsyncRead> AsyncRead for RecordingBufReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        caller_buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), Error>> {
        // if we don't have any buffered bytes, read some bytes into our buffer.
        let mut this = self.project();
        if *this.read_offset == this.buf.len() {
            this.buf.reserve(RECORDING_READER_BUF_SIZE);
            let mut read_buf = ReadBuf::uninit(this.buf.spare_capacity_mut());
            match this.reader.as_mut().poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    let bytes_read = read_buf.filled().len();
                    let new_len = this.buf.len() + bytes_read;
                    unsafe {
                        this.buf.set_len(new_len); // lol
                    }
                }
                rslt => return rslt,
            };
        }

        // read from the buffered bytes into the caller's buffer.
        let unread_bytes = &this.buf[*this.read_offset..];
        let n = min(caller_buf.remaining(), unread_bytes.len());
        caller_buf.put_slice(&unread_bytes[..n]);
        *this.read_offset += n;
        Poll::Ready(Ok(()))
    }
}

#[pin_project]
struct HandshakeRecordReader<R: AsyncRead> {
    #[pin]
    reader: R,
    currently_reading: HandshakeRecordReaderReading,
}

impl<R: AsyncRead> HandshakeRecordReader<R> {
    fn new(reader: R) -> HandshakeRecordReader<R> {
        HandshakeRecordReader {
            reader,
            currently_reading: HandshakeRecordReaderReading::ContentType,
        }
    }
}

enum HandshakeRecordReaderReading {
    ContentType,
    MajorMinorVersion(usize),
    RecordSize([u8; 2], usize),
    Record(usize),
}

impl<R: AsyncRead> AsyncRead for HandshakeRecordReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        caller_buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), Error>> {
        let mut this = self.project();
        loop {
            match this.currently_reading {
                HandshakeRecordReaderReading::ContentType => {
                    const CONTENT_TYPE_HANDSHAKE: u8 = 22;
                    let mut buf = [0];
                    let mut buf = ReadBuf::new(&mut buf[..]);
                    match this.reader.as_mut().poll_read(cx, &mut buf) {
                        Poll::Ready(Ok(())) if buf.filled().len() == 1 => {
                            if buf.filled()[0] != CONTENT_TYPE_HANDSHAKE {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!(
                                        "got wrong content type (wanted {}, got {})",
                                        CONTENT_TYPE_HANDSHAKE,
                                        buf.filled()[0]
                                    ),
                                )));
                            }
                            *this.currently_reading =
                                HandshakeRecordReaderReading::MajorMinorVersion(0);
                        }
                        rslt => return rslt,
                    }
                }

                HandshakeRecordReaderReading::MajorMinorVersion(bytes_read) => {
                    let mut buf = [0, 0];
                    let mut buf = ReadBuf::new(&mut buf[..]);
                    buf.advance(*bytes_read);
                    match this.reader.as_mut().poll_read(cx, &mut buf) {
                        Poll::Ready(Ok(())) => {
                            *bytes_read = buf.filled().len();
                            if *bytes_read == 2 {
                                *this.currently_reading =
                                    HandshakeRecordReaderReading::RecordSize([0, 0], 0);
                            }
                        }
                        rslt => return rslt,
                    }
                }

                HandshakeRecordReaderReading::RecordSize(backing_array, bytes_read) => {
                    const MAX_RECORD_SIZE: usize = 1 << 14;
                    let mut buf = ReadBuf::new(&mut backing_array[..]);
                    buf.advance(*bytes_read);
                    match this.reader.as_mut().poll_read(cx, &mut buf) {
                        Poll::Ready(Ok(())) => {
                            *bytes_read = buf.filled().len();
                            if *bytes_read == 2 {
                                let record_size = u16::from_be_bytes(*backing_array).into();
                                if record_size > MAX_RECORD_SIZE {
                                    return Poll::Ready(Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!(
                                            "record too large ({} > {})",
                                            record_size, MAX_RECORD_SIZE
                                        ),
                                    )));
                                }
                                *this.currently_reading =
                                    HandshakeRecordReaderReading::Record(record_size)
                            }
                        }
                        rslt => return rslt,
                    }
                }

                HandshakeRecordReaderReading::Record(remaining_record_bytes) => {
                    // We ultimately want to read record bytes into `caller_buf`, but we need to
                    // ensure that we don't read more bytes than there are record bytes (and end
                    // up handing the caller record header bytes). So we call `caller_buf.take()`.
                    // Since `take` returns an independent `ReadBuf`, we have to update `caller_buf`
                    // once we're done reading: first we call `assume_init` to assert that the
                    // `bytes_read` bytes we read are initialized, then we call `advance` to assert
                    // that the appropriate section of the buffer is filled.

                    let mut buf = caller_buf.take(*remaining_record_bytes);
                    let rslt = this.reader.as_mut().poll_read(cx, &mut buf);
                    if let Poll::Ready(Ok(())) = rslt {
                        let bytes_read = buf.filled().len();
                        unsafe {
                            caller_buf.assume_init(bytes_read);
                        }
                        caller_buf.advance(bytes_read);
                        *remaining_record_bytes -= bytes_read;
                        if *remaining_record_bytes == 0 {
                            *this.currently_reading = HandshakeRecordReaderReading::ContentType;
                        }
                    }
                    return rslt;
                }
            }
        }
    }
}

async fn read_sni_host_name_from_client_hello<R: AsyncRead>(
    mut reader: Pin<&mut R>,
) -> io::Result<String> {
    // Handshake message type.
    const HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 1;
    let typ = reader.read_u8().await?;
    if typ != HANDSHAKE_TYPE_CLIENT_HELLO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handshake message not a ClientHello (type {}, expected {})",
                typ, HANDSHAKE_TYPE_CLIENT_HELLO
            ),
        ));
    }

    // Handshake message length.
    let len = read_u24(reader.as_mut()).await?;
    let reader = reader.take(len.into());
    pin!(reader);

    // ProtocolVersion (2 bytes) & random (32 bytes).
    skip(reader.as_mut(), 34).await?;

    // Session ID (u8-length vec), cipher suites (u16-length vec), compression methods (u8-length vec).
    skip_vec_u8(reader.as_mut()).await?;
    skip_vec_u16(reader.as_mut()).await?;
    skip_vec_u8(reader.as_mut()).await?;

    // Extensions.
    let ext_len = reader.read_u16().await?;
    let new_limit = min(reader.limit(), ext_len.into());
    reader.set_limit(new_limit);
    loop {
        // Extension type & length.
        let ext_typ = reader.read_u16().await?;
        let ext_len = reader.read_u16().await?;

        const EXTENSION_TYPE_SNI: u16 = 0;
        if ext_typ != EXTENSION_TYPE_SNI {
            skip(reader.as_mut(), ext_len.into()).await?;
            continue;
        }
        let new_limit = min(reader.limit(), ext_len.into());
        reader.set_limit(new_limit);

        // ServerNameList length.
        let snl_len = reader.read_u16().await?;
        let new_limit = min(reader.limit(), snl_len.into());
        reader.set_limit(new_limit);

        // ServerNameList.
        loop {
            // NameType & length.
            let name_typ = reader.read_u8().await?;

            const NAME_TYPE_HOST_NAME: u8 = 0;
            if name_typ != NAME_TYPE_HOST_NAME {
                skip_vec_u16(reader.as_mut()).await?;
                continue;
            }

            let name_len = reader.read_u16().await?;
            let new_limit = min(reader.limit(), name_len.into());
            reader.set_limit(new_limit);
            let mut name_buf = vec![0; name_len.into()];
            reader.read_exact(&mut name_buf).await?;
            return String::from_utf8(name_buf).map_err(|err| io::Error::new(ErrorKind::InvalidData, err));
        }
    }
}

async fn skip<R: AsyncRead>(reader: Pin<&mut R>, len: u64) -> io::Result<()> {
    let bytes_read = io::copy(&mut reader.take(len), &mut io::sink()).await?;
    if bytes_read < len {
        return Err(io::Error::new(ErrorKind::UnexpectedEof, format!("skip read {} < {} bytes", bytes_read, len)));
    }
    Ok(())
}

async fn skip_vec_u8<R: AsyncRead>(mut reader: Pin<&mut R>) -> io::Result<()> {
    let sz = reader.read_u8().await?;
    skip(reader.as_mut(), sz.into()).await
}

async fn skip_vec_u16<R: AsyncRead>(mut reader: Pin<&mut R>) -> io::Result<()> {
    let sz = reader.read_u16().await?;
    skip(reader.as_mut(), sz.into()).await
}

async fn read_u24<R: AsyncRead>(mut reader: Pin<&mut R>) -> io::Result<u32> {
    let mut buf = [0; 3];
    reader
        .as_mut()
        .read_exact(&mut buf)
        .await
        .map(|_| NetworkEndian::read_u24(&buf))
}

// create the endpoint, proxy.threethain.dev/did-123, proxy.threethain.xyz?new
// create a new clent manager, the manager should listen on the assigned port
// send request to the custom domain, get client id
// get the client manager with client id
// client manager handle the request.
