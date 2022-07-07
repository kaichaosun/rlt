use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LinesCodec};

/// Timeout for network connections and initial protocol messages.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

/// Maxmium byte length for a JSON frame in the stream.
pub const MAX_FRAME_LENGTH: usize = 1024;

#[derive(Debug, Serialize, Deserialize)]
struct ProxyResponse {
    id: String,
    port: u16,
    max_conn_count: u8,
    url: String,
}

pub async fn open_tunnel(
    server: &str,
    subdomain: Option<&str>,
    local_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("start connect to: {}, {}", "localhost", "12000");

    // Get custome domain
    let assigned_domain = subdomain.unwrap_or("?new");
    let uri = format!("{}/{}", server, assigned_domain);
    println!("assigned domain: {}", uri);
    let resp = reqwest::get(uri).await?.json::<ProxyResponse>().await?;
    println!("{:#?}", resp);

    // connect to remote host
    let remote_stream = TcpStream::connect(format!("proxy.ad4m.dev:{}", resp.port)).await?;
    println!("remote stream connectted");

    // let codec = AnyDelimiterCodec::new_with_max_length(vec![0], vec![0], MAX_FRAME_LENGTH);
    let codec = LinesCodec::new();

    let mut framed_stream = Framed::new(remote_stream, codec);

    let counter = Arc::new(Mutex::new(0));

    loop {
        let _message = framed_stream.next().await;
        // println!("messages comes in: {:?}", message);

        let mut locked_counter = counter.lock().unwrap();
        if *locked_counter < resp.max_conn_count {
            println!("spawn new proxy");
            *locked_counter += 1;

            let counter2 = Arc::clone(&counter);
            tokio::spawn(async move { handle_conn(resp.port, local_port, counter2).await });
        }
    }
}

async fn handle_conn(remote_port: u16, local_port: u16, counter: Arc<Mutex<u8>>) -> Result<()> {
    let remote_stream_in = TcpStream::connect(format!("proxy.ad4m.dev:{}", remote_port)).await?;

    let local_stream_in = TcpStream::connect(format!("127.0.0.1:{}", local_port)).await?;

    proxy(remote_stream_in, local_stream_in, counter).await?;
    Ok(())
}

/// Copy data mutually between two read/write streams.
pub async fn proxy<S1, S2>(stream1: S1, stream2: S2, counter: Arc<Mutex<u8>>) -> io::Result<()>
where
    S1: AsyncRead + AsyncWrite + Unpin,
    S2: AsyncRead + AsyncWrite + Unpin,
{
    let (mut s1_read, mut s1_write) = io::split(stream1);
    let (mut s2_read, mut s2_write) = io::split(stream2);
    tokio::select! {
        res = io::copy(&mut s1_read, &mut s2_write) => res,
        res = io::copy(&mut s2_read, &mut s1_write) => res,
    }?;
    let mut locked_counter = counter.lock().unwrap();
    *locked_counter -= 1;

    Ok(())
}

// async fn connect_with_timeout(to: &str, port: u16) -> Result<TcpStream> {
//     match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
//         Ok(res) => res,
//         Err(err) => Err(err.into()),
//     }
//     .with_context(|| format!("could not connect to {to}:{port}"))
// }

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
