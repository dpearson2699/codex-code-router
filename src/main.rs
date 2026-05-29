use clap::{Parser, Subcommand};
use codex_code_router::config::AppConfig;
use codex_code_router::proxy::serve;
use codex_code_router::token::printable_token_from_auth_config;
use std::io::{self, Write};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "codex-code-router")]
#[command(about = "Local Codex -> GitHub Copilot Responses API adapter")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local HTTP adapter service.
    Serve,
    /// Print the configured Copilot bearer token only.
    PrintToken,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            init_tracing();
            serve(AppConfig::from_env()).await
        }
        Command::PrintToken => print_token(),
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
}

fn print_token() -> anyhow::Result<()> {
    let config = AppConfig::from_env();
    match printable_token_from_auth_config(&config.auth) {
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
