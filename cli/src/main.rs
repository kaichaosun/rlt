use clap::{Parser, Subcommand};
use localtunnel::{open_tunnel, broadcast};
use localtunnel_server::create;
use tokio::signal;

mod config;

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
        #[clap(long, default_value = "localhost")]
        local_host: String,
        /// The local port to expose.
        #[clap(short, long)]
        port: u16,
        /// Max connections allowed to server.
        #[clap(long, default_value = "10")]
        max_conn: u8,
    },

    /// Starts proxy server to accept user connections and proxy setup connection.
    Server {
        /// Domain name of the proxy server, required if use subdomain like lt.example.com.
        #[clap(long)]
        domain: String,
        /// The port to accept initialize proxy endpoint.
        #[clap(short, long, default_value = "3000")]
        port: u16,
        /// The flag to indicate proxy over https.
        #[clap(long, default_value = "false")]
        secure: bool,
        /// Maximum number of tcp sockets each client to establish at one time.
        #[clap(long, default_value = "10")]
        max_sockets: u8,
        /// The port to accept user request for proxying.
        #[clap(long, default_value = "3001")]
        proxy_port: u16,
    },
}

#[tokio::main]
async fn main() {
    config::setup();
    log::info!("Run localtunnel CLI!");

    let command = Cli::parse().command;

    match command {
        Command::Client {
            host,
            subdomain,
            local_host,
            port,
            max_conn,
        } => {
            let (notify_shutdown, _) = broadcast::channel(1);
            let result = open_tunnel(
                Some(&host),
                Some(&subdomain),
                Some(&local_host),
                port,
                notify_shutdown.clone(),
                max_conn,
            )
            .await
            .unwrap();
            log::info!("Tunnel url: {:?}", result);

            signal::ctrl_c().await.expect("failed to listen for event");
            log::info!("Quit");
        }
        Command::Server {
            domain,
            port,
            secure,
            max_sockets,
            proxy_port,
        } => {
            create(domain, port, secure, max_sockets, proxy_port).await
        }
    }
}
