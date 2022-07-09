use localtunnel::open_tunnel;

#[tokio::main]
async fn main() {
    println!("Run localtunnel CLI!");

    let result = open_tunnel(
        Some("http://proxy.ad4m.dev"),
        Some("did-key-zq3shddyxbs38frgusjrwswc7t21jcooequddaytptsrtyaqk"), 
        None, 
        12000,
    ).await.unwrap();
    println!("result: {:?}", result.0);
    
    let _ = tokio::spawn(async move {
        let _ = result.1.await;
    }).await;
    
}
