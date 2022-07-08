use localtunnel::open_tunnel;

#[tokio::main]
async fn main() {
    println!("Run localtunnel CLI!");

    let result = open_tunnel(Some("http://proxy.ad4m.dev"), Some("kaichao"), None, 12000).await;

    println!("result: {:?}", result);
}
