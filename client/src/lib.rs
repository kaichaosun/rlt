use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::codec::{AnyDelimiterCodec, Framed};

/// Timeout for network connections and initial protocol messages.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

/// Maxmium byte length for a JSON frame in the stream.
pub const MAX_FRAME_LENGTH: usize = 256;

#[derive(Debug, Serialize, Deserialize)]
struct ProxyResponse {
    id: String,
    port: u16,
    max_conn_count: u8,
    url: String,
}

pub async fn open_tunnel(
    host: &str,
    subdomain: Option<&str>,
    local_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("start connect to: {}, {}", "localhost", "12000");

    // connect to remote host

    // connect to local port

    // forward the traffic from remote host to local

    // Get custome domain
    let assigned_domain = subdomain.unwrap_or("?new");
    let uri = format!("{}/{}", host, assigned_domain);
    println!("assigned domain: {}", uri);
    let resp = reqwest::get(uri).await?.json::<ProxyResponse>().await?;
    println!("{:#?}", resp);

    // connect to remote host
    let mut remote_stream = TcpStream::connect(format!("proxy.ad4m.dev:{}", resp.port)).await?;

    // connect to local port
    // let mut local_stream = TcpStream::connect(format!("127.0.0.1:{}", local_port)).await?;

    let codec = AnyDelimiterCodec::new_with_max_length(vec![0], vec![0], MAX_FRAME_LENGTH);

    let mut framed_stream = Framed::new(remote_stream, codec);

    loop {
        if let Some(message) = framed_stream.next().await {
            tokio::spawn(async move {
                handle_conn(resp.port, local_port).await
            });
        }
    }

    Ok(())
}

async fn handle_conn(remote_port: u16, local_port: u16) -> Result<()> {
    let mut remote_stream_in = TcpStream::connect(format!("proxy.ad4m.dev:{}", remote_port)).await?;

    // connect to local port
    let mut local_stream_in = TcpStream::connect(format!("127.0.0.1:{}", local_port)).await?;

    let codec_in = AnyDelimiterCodec::new_with_max_length(vec![0], vec![0], MAX_FRAME_LENGTH);
    let mut framed_stream_in = Framed::new(remote_stream_in, codec_in);

    let parts = framed_stream_in.into_parts();

    proxy(local_stream_in, parts.io).await?;
    Ok(())
}

/// Copy data mutually between two read/write streams.
pub async fn proxy<S1, S2>(stream1: S1, stream2: S2) -> io::Result<()>
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
    Ok(())
}

async fn connect_with_timeout(to: &str, port: u16) -> Result<TcpStream> {
    match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
