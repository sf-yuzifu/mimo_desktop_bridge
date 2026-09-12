use clap::{Parser, Subcommand};
use mimo_desktop_bridge_lib::server::{start_http, ServerConfig};
use mimo_desktop_bridge_lib::{BridgeState, Storage};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "mimo_desktop_bridge",
    about = "Bridge Xiaomi MiMo Desktop free account channel into local OpenAI-compatible APIs",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the headless HTTP server (WebUI + /v1)
    Server {
        /// Listen port (default from settings, else 8787)
        #[arg(long)]
        port: Option<u16>,
        /// Listen host (default 127.0.0.1)
        #[arg(long)]
        host: Option<String>,
        /// Open the WebUI in the default browser
        #[arg(long)]
        open: bool,
        /// Override config directory
        #[arg(long)]
        config_dir: Option<std::path::PathBuf>,
        /// Serve HTTPS (self-signed cert auto-generated under config dir)
        #[arg(long)]
        tls: bool,
        /// PEM certificate (implies --tls)
        #[arg(long)]
        tls_cert: Option<std::path::PathBuf>,
        /// PEM private key (implies --tls)
        #[arg(long)]
        tls_key: Option<std::path::PathBuf>,
    },
    /// Print auth / proxy status
    Status {
        #[arg(long)]
        config_dir: Option<std::path::PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mimo_desktop_bridge_lib=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Commands::Server {
        port: None,
        host: None,
        open: false,
        config_dir: None,
        tls: false,
        tls_cert: None,
        tls_key: None,
    }) {
        Commands::Server {
            port,
            host,
            open,
            config_dir,
            tls,
            tls_cert,
            tls_key,
        } => {
            let storage = match config_dir {
                Some(d) => Arc::new(Storage::open_in(d)?),
                None => Arc::new(Storage::open()?),
            };
            let settings = storage.settings();
            let port = port.unwrap_or(settings.port);
            let host: IpAddr = host
                .map(|h| h.parse().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)))
                .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
            let tls_flag = if tls || tls_cert.is_some() || tls_key.is_some() {
                Some(true)
            } else {
                None
            };

            let state = Arc::new(BridgeState::new(storage.clone()));
            let server = start_http(
                state,
                ServerConfig {
                    host,
                    port,
                    tls: tls_flag,
                    tls_cert,
                    tls_key,
                },
            )
            .await?;

            println!("mimo_desktop_bridge listening on {}", server.webui_url());
            println!("  webui:    {}", server.webui_url());
            println!("  models:   {}/v1/models", server.webui_url());
            println!("  chat:     {}/v1/chat/completions", server.webui_url());
            println!("  messages: {}/v1/messages", server.webui_url());
            println!("  config:   {}", storage.config_dir().display());
            println!("  sid:      {}", mimo_desktop_bridge_lib::auth::SID);
            println!("  upstream: {}", mimo_desktop_bridge_lib::auth::API_BASE);

            if open {
                let _ = open::that(server.webui_url());
            }

            tokio::signal::ctrl_c().await?;
            println!("\nshutting down…");
            server.shutdown().await;
        }
        Commands::Status { config_dir } => {
            let storage = match config_dir {
                Some(d) => Storage::open_in(d)?,
                None => Storage::open()?,
            };
            let s = storage.session();
            let st = storage.settings();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "configDir": storage.config_dir(),
                    "adminConfigured": storage.admin_configured(),
                    "apiKeyRequired": st.api_key_required,
                    "port": st.port,
                    "tlsEnabled": st.tls_enabled,
                    "loggedIn": s.as_ref().map(|x| x.is_authenticated()).unwrap_or(false),
                    "userId": s.as_ref().and_then(|x| x.user_id.clone()),
                    "hasPassToken": s.as_ref().and_then(|x| x.pass_token.as_ref().map(|_| true)).unwrap_or(false),
                    "hasServiceToken": s.as_ref().and_then(|x| x.service_token.as_ref().map(|_| true)).unwrap_or(false),
                    "apiKeys": storage.list_keys().len(),
                }))?
            );
        }
    }
    Ok(())
}
