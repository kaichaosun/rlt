use clap::{Parser, Subcommand};
use localtunnel::open_tunnel;
use tokio::{sync::broadcast, signal};

#[derive(Parser)]
#[clap(author, version, about)]
#[clap(propagate_version = true)]
struct Cli {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Builds connection between remote proxy server and local api.
    Client {
        /// Address of proxy server
        #[clap(long)]
        host: String,
        /// Subdomain of the proxied url
        #[clap(long)]
        subdomain: String,
        /// The local host to expose.
        #[clap(long, value_name = "HOST", default_value = "localhost")]
        local_host: String,
        /// The local port to expose.
        #[clap(short, long)]
        port: u16,
    },

    /// Starts proxy server to accept user connections and proxy setup connection.
    Server {},
}

#[tokio::main]
async fn main() {
    println!("Run localtunnel CLI!");

    let command = Cli::parse().command;

    match command {
        Command::Client {
            host,
            subdomain,
            local_host,
            port,
        } => {
            let (notify_shutdown, _) = broadcast::channel(1);
            let result = open_tunnel(
                Some(&host),
                Some(&subdomain),
                Some(&local_host),
                port,
                notify_shutdown.clone(),
            )
            .await
            .unwrap();
            println!("result: {:?}", result);

            signal::ctrl_c().await.expect("failed to listen for event");
            println!("Quit");
        }
        Command::Server {} => {
            println!("Not implemented.")
        }
    }
}
