use crate::config::AppConfig;
use crate::proxy::serve;
use crate::redaction::truncate_for_log;
use crate::service_control::{restart_service, start_service, status_service, stop_service};
use crate::token::{
    login_with_device_flow, printable_token_from_auth_config,
    should_try_device_login_after_auth_error,
};
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use reqwest::header::USER_AGENT;
use serde_json::Value;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

const DEFAULT_LOG_FILTER: &str = "codex_code_router=info,warn";
const DEFAULT_UPDATE_CRATE: &str = "codex-code-router";
const DEFAULT_UPDATE_REPO: &str = "https://github.com/dpearson2699/codex-code-router";
const UPDATE_USER_AGENT: &str = concat!("codex-code-router/", env!("CARGO_PKG_VERSION"));

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
    /// Update installed binaries from crates.io and restart the service.
    Update(UpdateArgs),
    /// Run interactive GitHub Copilot device login and save the token file.
    Login,
    /// Print the configured Copilot bearer token only.
    PrintToken,
}

#[derive(Debug, Args)]
struct UpdateArgs {
    /// Crates.io package to install from.
    #[arg(long = "crate", default_value = DEFAULT_UPDATE_CRATE)]
    crate_name: String,
    /// Install a specific crates.io package version instead of the latest version.
    #[arg(long, conflicts_with_all = ["repo", "tag", "branch"])]
    version: Option<String>,
    /// Git repository to install from instead of crates.io.
    #[arg(long)]
    repo: Option<String>,
    /// Install a specific Git tag instead of the latest GitHub release.
    #[arg(long, conflicts_with_all = ["branch", "version"])]
    tag: Option<String>,
    /// Install a branch instead of the latest GitHub release.
    #[arg(long, conflicts_with_all = ["tag", "version"])]
    branch: Option<String>,
    /// Install binaries without restarting the background service.
    #[arg(long)]
    no_restart: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UpdateRef {
    CratesIo {
        crate_name: String,
        version: Option<String>,
    },
    Git {
        repo: String,
        git_ref: GitUpdateRef,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GitUpdateRef {
    Tag(String),
    Branch(String),
}

impl UpdateRef {
    fn description(&self) -> String {
        match self {
            Self::CratesIo {
                crate_name,
                version: Some(version),
            } => format!("crates.io package {crate_name}@{version}"),
            Self::CratesIo {
                crate_name,
                version: None,
            } => format!("latest crates.io package {crate_name}"),
            Self::Git { repo, git_ref } => format!("{} from {repo}", git_ref.description()),
        }
    }
}

impl GitUpdateRef {
    fn description(&self) -> String {
        match self {
            Self::Tag(tag) => format!("tag {tag}"),
            Self::Branch(branch) => format!("branch {branch}"),
        }
    }
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            init_tracing();
            let config = AppConfig::from_env();
            ensure_auth_ready_or_login(&config).await?;
            serve(config).await
        }
        Command::Start => {
            let config = AppConfig::from_env();
            ensure_auth_ready_or_login(&config).await?;
            start_service(&config)
        }
        Command::Stop => stop_service(&AppConfig::from_env()),
        Command::Restart => {
            let config = AppConfig::from_env();
            ensure_auth_ready_or_login(&config).await?;
            restart_service(&config)
        }
        Command::Status => status_service(&AppConfig::from_env()),
        Command::Update(args) => update(args).await,
        Command::Login => login().await,
        Command::PrintToken => print_token().await,
    }
}

async fn update(args: UpdateArgs) -> anyhow::Result<()> {
    let update_ref = resolve_update_ref(&args).await?;
    println!("updating ccrx from {}", update_ref.description());
    cargo_install_update(&update_ref)?;
    println!("installed updated ccrx binaries");

    if args.no_restart {
        println!("skipped restart; run `ccrx restart` when you want Codex to use the update");
        return Ok(());
    }

    let config = AppConfig::from_env();
    ensure_auth_ready_or_login(&config).await?;
    restart_service(&config)
}

async fn resolve_update_ref(args: &UpdateArgs) -> Result<UpdateRef> {
    let repo = args
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty());

    if let Some(tag) = args
        .tag
        .as_deref()
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
    {
        return Ok(UpdateRef::Git {
            repo: repo.unwrap_or(DEFAULT_UPDATE_REPO).to_owned(),
            git_ref: GitUpdateRef::Tag(tag.to_owned()),
        });
    }
    if let Some(branch) = args
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
    {
        return Ok(UpdateRef::Git {
            repo: repo.unwrap_or(DEFAULT_UPDATE_REPO).to_owned(),
            git_ref: GitUpdateRef::Branch(branch.to_owned()),
        });
    }

    if let Some(repo) = repo {
        return fetch_latest_release_tag(repo)
            .await
            .map(|tag| UpdateRef::Git {
                repo: repo.to_owned(),
                git_ref: GitUpdateRef::Tag(tag),
            });
    }

    Ok(UpdateRef::CratesIo {
        crate_name: args.crate_name.trim().to_owned(),
        version: args
            .version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .map(ToOwned::to_owned),
    })
}

async fn fetch_latest_release_tag(repo: &str) -> Result<String> {
    let api_url = github_latest_release_api_url(repo)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .get(&api_url)
        .header(USER_AGENT, UPDATE_USER_AGENT)
        .send()
        .await
        .with_context(|| format!("failed to fetch latest GitHub release from {api_url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "failed to fetch latest GitHub release from {api_url}: HTTP {status}. Create a GitHub release, pass `--tag <tag>`, or use `--branch main`. {}",
            truncate_for_log(&body, 240)
        );
    }

    let value: Value = response
        .json()
        .await
        .with_context(|| format!("failed to parse GitHub release response from {api_url}"))?;
    value
        .get("tag_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("GitHub latest release response did not include tag_name"))
}

fn github_latest_release_api_url(repo: &str) -> Result<String> {
    let repo = repo.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = if let Some(path) = repo.strip_prefix("https://github.com/") {
        path
    } else if let Some(path) = repo.strip_prefix("http://github.com/") {
        path
    } else if let Some(path) = repo.strip_prefix("git@github.com:") {
        path
    } else {
        bail!(
            "cannot discover latest GitHub release for `{repo}`. Use a github.com repo URL, pass `--tag <tag>`, or use `--branch main`."
        );
    };

    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() {
        bail!("invalid GitHub repo URL `{repo}`");
    }

    Ok(format!(
        "https://api.github.com/repos/{owner}/{name}/releases/latest"
    ))
}

fn cargo_install_update(update_ref: &UpdateRef) -> Result<()> {
    let install_root = current_install_root();
    if let Some(root) = &install_root {
        println!("install root: {}", root.display());
    } else {
        println!("install root: cargo default");
    }

    let args = cargo_install_args(update_ref, install_root.as_deref());
    println!("running: cargo {}", args.join(" "));

    let status = ProcessCommand::new("cargo")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run cargo install")?;

    if status.success() {
        Ok(())
    } else {
        bail!("cargo install failed with status {status}")
    }
}

fn current_install_root() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| install_root_from_exe_path(&exe))
}

fn install_root_from_exe_path(exe: &Path) -> Option<PathBuf> {
    let bin_dir = exe.parent()?;
    if bin_dir.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return None;
    }

    bin_dir.parent().map(Path::to_path_buf)
}

fn cargo_install_args(update_ref: &UpdateRef, install_root: Option<&Path>) -> Vec<String> {
    let mut args = vec!["install".to_owned()];
    match update_ref {
        UpdateRef::CratesIo {
            crate_name,
            version: Some(version),
        } => args.push(format!("{crate_name}@{version}")),
        UpdateRef::CratesIo {
            crate_name,
            version: None,
        } => args.push(crate_name.to_owned()),
        UpdateRef::Git { repo, git_ref } => {
            args.extend(["--git".to_owned(), repo.to_owned()]);
            match git_ref {
                GitUpdateRef::Tag(tag) => args.extend(["--tag".to_owned(), tag.to_owned()]),
                GitUpdateRef::Branch(branch) => {
                    args.extend(["--branch".to_owned(), branch.to_owned()])
                }
            }
        }
    }
    if let Some(root) = install_root {
        args.extend(["--root".to_owned(), root.display().to_string()]);
    }
    args.extend([
        "--bins".to_owned(),
        "--locked".to_owned(),
        "--force".to_owned(),
    ]);
    args
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(false)
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

async fn login() -> anyhow::Result<()> {
    let config = AppConfig::from_env();
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()?;
    let mut stderr = io::stderr();
    login_with_device_flow(&config.auth, &config.headers, &client, &mut stderr).await?;
    Ok(())
}

async fn ensure_auth_ready_or_login(config: &AppConfig) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()?;

    match printable_token_from_auth_config(&config.auth, &config.headers, &client).await {
        Ok(_) => Ok(()),
        Err(error) if should_try_device_login_after_auth_error(&error) => {
            eprintln!("Copilot auth is unavailable: {error}");
            let mut stderr = io::stderr();
            login_with_device_flow(&config.auth, &config.headers, &client, &mut stderr).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_latest_release_api_url_accepts_https_repo_urls() {
        assert_eq!(
            github_latest_release_api_url("https://github.com/dpearson2699/codex-code-router.git")
                .unwrap(),
            "https://api.github.com/repos/dpearson2699/codex-code-router/releases/latest"
        );
    }

    #[test]
    fn github_latest_release_api_url_accepts_ssh_repo_urls() {
        assert_eq!(
            github_latest_release_api_url("git@github.com:dpearson2699/codex-code-router.git")
                .unwrap(),
            "https://api.github.com/repos/dpearson2699/codex-code-router/releases/latest"
        );
    }

    #[test]
    fn cargo_install_args_use_crates_io_by_default() {
        assert_eq!(
            cargo_install_args(
                &UpdateRef::CratesIo {
                    crate_name: DEFAULT_UPDATE_CRATE.to_owned(),
                    version: None,
                },
                None,
            ),
            vec![
                "install",
                DEFAULT_UPDATE_CRATE,
                "--bins",
                "--locked",
                "--force",
            ]
        );
    }

    #[test]
    fn cargo_install_args_can_pin_crates_io_version() {
        assert_eq!(
            cargo_install_args(
                &UpdateRef::CratesIo {
                    crate_name: DEFAULT_UPDATE_CRATE.to_owned(),
                    version: Some("0.1.1".to_owned()),
                },
                None,
            ),
            vec![
                "install",
                "codex-code-router@0.1.1",
                "--bins",
                "--locked",
                "--force",
            ]
        );
    }

    #[test]
    fn cargo_install_args_can_use_git_release_tag() {
        assert_eq!(
            cargo_install_args(
                &UpdateRef::Git {
                    repo: DEFAULT_UPDATE_REPO.to_owned(),
                    git_ref: GitUpdateRef::Tag("v0.1.0".to_owned()),
                },
                None,
            ),
            vec![
                "install",
                "--git",
                DEFAULT_UPDATE_REPO,
                "--tag",
                "v0.1.0",
                "--bins",
                "--locked",
                "--force",
            ]
        );
    }

    #[test]
    fn cargo_install_args_can_use_git_branch() {
        assert_eq!(
            cargo_install_args(
                &UpdateRef::Git {
                    repo: DEFAULT_UPDATE_REPO.to_owned(),
                    git_ref: GitUpdateRef::Branch("main".to_owned()),
                },
                None,
            ),
            vec![
                "install",
                "--git",
                DEFAULT_UPDATE_REPO,
                "--branch",
                "main",
                "--bins",
                "--locked",
                "--force",
            ]
        );
    }

    #[test]
    fn cargo_install_args_preserve_detected_install_root() {
        assert_eq!(
            cargo_install_args(
                &UpdateRef::CratesIo {
                    crate_name: DEFAULT_UPDATE_CRATE.to_owned(),
                    version: None,
                },
                Some(Path::new("/Users/example/.cargo")),
            ),
            vec![
                "install",
                DEFAULT_UPDATE_CRATE,
                "--root",
                "/Users/example/.cargo",
                "--bins",
                "--locked",
                "--force",
            ]
        );
    }

    #[test]
    fn install_root_from_exe_path_uses_parent_of_bin_dir() {
        assert_eq!(
            install_root_from_exe_path(Path::new("/Users/example/.cargo/bin/ccrx")),
            Some(PathBuf::from("/Users/example/.cargo"))
        );
    }

    #[test]
    fn install_root_from_exe_path_ignores_non_bin_paths() {
        assert_eq!(
            install_root_from_exe_path(Path::new("/repo/target/debug/ccrx")),
            None
        );
    }
}
