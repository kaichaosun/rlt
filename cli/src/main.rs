use localtunnel::open_tunnel;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() {
    println!("Run localtunnel CLI!");

    let (notify_shutdown, _) = broadcast::channel(1);
    let result = open_tunnel(
        Some("http://proxy.ad4m.dev"),
        Some("did-key-zq3shddyxbs38frgusjrwswc7t21jcooequddaytptsrtyaqk"), 
        None, 
        12000,
        notify_shutdown.clone(),
    ).await.unwrap();
    println!("result: {:?}", result);

    sleep(Duration::from_millis(30000)).await;
    let _ = notify_shutdown.send(());

    sleep(Duration::from_millis(100000)).await;

}
