use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize)]
struct ProxyResponse {
    id: String,
    port: u32,
    max_conn_count: u8,
    url: String,
}

pub async fn open_tunnel(host: &str, subdomain: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
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
