use anyhow::Result;
use clap::{Parser, Subcommand};
use localtunnel_client::{broadcast, open_tunnel, ClientConfig};
use localtunnel_server::{start, ServerConfig};
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
        #[clap(long, default_value = "127.0.0.1")]
        local_host: String,
        /// The local port to expose.
        #[clap(short, long)]
        port: u16,
        /// Max connections allowed to server.
        #[clap(long, default_value = "10")]
        max_conn: u8,
        #[clap(long)]
        credential: Option<String>,
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
        #[clap(long)]
        secure: bool,
        /// Maximum number of tcp sockets each client to establish at one time.
        #[clap(long, default_value = "10")]
        max_sockets: u8,
        /// The port to accept user request for proxying.
        #[clap(long, default_value = "3001")]
        proxy_port: u16,
        #[clap(long)]
        require_auth: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
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
            credential,
        } => {
            let (notify_shutdown, _) = broadcast::channel(1);
            let config = ClientConfig {
                server: Some(host),
                subdomain: Some(subdomain),
                local_host: Some(local_host),
                local_port: port,
                shutdown_signal: notify_shutdown.clone(),
                max_conn,
                credential,
            };
            let result = open_tunnel(config).await?;
            log::info!("Tunnel url: {:?}", result);

            signal::ctrl_c().await?;
            log::info!("Quit");
        }
        Command::Server {
            domain,
            port,
            secure,
            max_sockets,
            proxy_port,
            require_auth,
        } => {
            let config = ServerConfig {
                domain,
                api_port: port,
                secure,
                max_sockets,
                proxy_port,
                require_auth,
            };
            start(config).await?;
        }
    }

    Ok(())
}
