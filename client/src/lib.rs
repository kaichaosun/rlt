use serde::{Serialize, Deserialize};
use tokio::net::TcpStream;
use tokio::io;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Serialize, Deserialize)]
struct ProxyResponse {
    id: String,
    port: u16,
    max_conn_count: u8,
    url: String,
}

pub async fn open_tunnel(host: &str, subdomain: Option<&str>, local_port: u16) -> Result<(), Box<dyn std::error::Error>> {
    println!("start connect to: {}, {}", "localhost", "12000");

    // connect to remote host

    // connect to local port

    // forward the traffic from remote host to local


    // Get custome domain
    let assigned_domain = subdomain.unwrap_or("?new");
    let uri = format!("{}/{}", host, assigned_domain);
    println!("assigned domain: {}", uri);
    let resp = reqwest::get(uri)
        .await?
        .json::<ProxyResponse>()
        .await?;
    println!("{:#?}", resp);

    println!("ending");

    // connect to remote host
    let mut remote_stream = TcpStream::connect(format!("proxy.ad4m.dev:{}", resp.port)).await?;

    let mut local_stream = TcpStream::connect(format!("127.0.0.1:{}", local_port)).await?;

    loop {
        remote_stream.readable().await?;

        let (mut read_remote, mut write_remote) = remote_stream.split();
        let (mut read_local, mut write_local) = local_stream.split();
        let proxy_to_local = async {
            io::copy(&mut read_remote, &mut write_local).await?;
            write_local.shutdown().await
        };
    
        let local_to_proxy = async {
            io::copy(&mut read_local, &mut write_remote).await?;
            write_remote.shutdown().await
        };

        tokio::try_join!(proxy_to_local, local_to_proxy)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
