use crate::config::AppConfig;
use crate::proxy::serve;
use crate::service_control::{restart_service, start_service, status_service, stop_service};
use crate::token::printable_token_from_auth_config;
use clap::{Parser, Subcommand};
use std::io::{self, Write};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(about = "Local Codex -> GitHub Copilot Responses API adapter")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local HTTP adapter service in the foreground.
    Serve,
    /// Start the local adapter service in the background.
    Start,
    /// Stop the background adapter service.
    Stop,
    /// Restart the background adapter service.
    Restart,
    /// Show whether the local adapter service is reachable.
    Status,
    /// Print the configured Copilot bearer token only.
    PrintToken,
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            init_tracing();
            serve(AppConfig::from_env()).await
        }
        Command::Start => start_service(&AppConfig::from_env()),
        Command::Stop => stop_service(&AppConfig::from_env()),
        Command::Restart => restart_service(&AppConfig::from_env()),
        Command::Status => status_service(&AppConfig::from_env()),
        Command::PrintToken => print_token().await,
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
}

async fn print_token() -> anyhow::Result<()> {
    let config = AppConfig::from_env();
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()?;
    match printable_token_from_auth_config(&config.auth, &config.headers, &client).await {
        Ok(token) => {
            io::stdout().write_all(token.as_bytes())?;
            Ok(())
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
