use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use nntp_proxy::{NntpProxy, load_config, create_default_config};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "8119")]
    port: u16,

    /// Configuration file path
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Load configuration
    let config = match load_config(&args.config).or_else(|_| {
        warn!("Config file not found, creating default config");
        let default_config = create_default_config();
        let config_toml = toml::to_string_pretty(&default_config)?;
        std::fs::write(&args.config, &config_toml)?;
        info!("Created default config file: {}", args.config);
        Ok(default_config)
    }) {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to load or create config: {}", e);
            return Err(e);
        }
    };
    
    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Create proxy
    let proxy = NntpProxy::new(config)?;

    // Start listening
    let listen_addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&listen_addr).await?;
    info!("NNTP proxy listening on {}", listen_addr);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let proxy_clone = proxy.clone();
                tokio::spawn(async move {
                    if let Err(e) = proxy_clone.handle_client(stream, addr).await {
                        error!("Error handling client {}: {}", addr, e);
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}
