use localtunnel::open_tunnel;
use tokio::task::JoinHandle;

#[tokio::main]
async fn main() {
    println!("Run localtunnel CLI!");

    let mut handles: Vec<JoinHandle<()>> = vec![];

    let result = open_tunnel(
        Some("http://proxy.ad4m.dev"),
        Some("kaichao"), 
        None, 
        12000,
        &mut handles
    ).await;
    println!("result: {:?}", result);
    
    futures::future::join_all(handles).await;
}
