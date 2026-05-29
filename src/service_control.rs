use crate::config::AppConfig;
use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const STATE_DIR_ENV: &str = "CODEX_CODE_ROUTER_STATE_DIR";
const DEFAULT_STATE_DIR: &str = ".codex-code-router";
const PID_FILE: &str = "codex-code-router.pid";
const LOG_FILE: &str = "codex-code-router.log";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicePaths {
    pub state_dir: PathBuf,
    pub pid_file: PathBuf,
    pub log_file: PathBuf,
}

impl ServicePaths {
    pub fn from_env() -> Result<Self> {
        let state_dir = match env::var_os(STATE_DIR_ENV) {
            Some(path) => PathBuf::from(path),
            None => home_dir()?.join(DEFAULT_STATE_DIR),
        };

        Ok(Self::from_state_dir(state_dir))
    }

    pub fn from_state_dir(state_dir: PathBuf) -> Self {
        Self {
            pid_file: state_dir.join(PID_FILE),
            log_file: state_dir.join(LOG_FILE),
            state_dir,
        }
    }
}

pub fn start_service(config: &AppConfig) -> Result<()> {
    let paths = ServicePaths::from_env()?;
    start_service_with_paths(config, &paths)
}

pub fn stop_service(config: &AppConfig) -> Result<()> {
    let paths = ServicePaths::from_env()?;
    stop_service_with_paths(config, &paths)
}

pub fn restart_service(config: &AppConfig) -> Result<()> {
    let paths = ServicePaths::from_env()?;
    stop_service_with_paths(config, &paths)?;
    start_service_with_paths(config, &paths)
}

pub fn status_service(config: &AppConfig) -> Result<()> {
    let paths = ServicePaths::from_env()?;
    let endpoint = endpoint(config);
    let health = health_check(config);
    let pid = read_pid(&paths)?;

    match (health, pid) {
        (true, Some(pid)) if process_exists(pid) => {
            println!("ccrx is running at {endpoint} (pid {pid})");
            println!("log: {}", paths.log_file.display());
        }
        (true, Some(pid)) => {
            println!("ccrx is reachable at {endpoint}, but pid file {pid} is stale");
            println!("log: {}", paths.log_file.display());
        }
        (true, None) => {
            println!("ccrx is reachable at {endpoint}, but no pid file exists");
        }
        (false, Some(pid)) if process_exists(pid) => {
            println!("ccrx process {pid} exists, but health check failed at {endpoint}");
            println!("log: {}", paths.log_file.display());
        }
        _ => println!("ccrx is stopped"),
    }

    Ok(())
}

fn start_service_with_paths(config: &AppConfig, paths: &ServicePaths) -> Result<()> {
    let endpoint = endpoint(config);
    if health_check(config) {
        println!("ccrx is already running at {endpoint}");
        return Ok(());
    }

    if let Some(pid) = read_pid(paths)? {
        if process_exists(pid) {
            bail!(
                "pid file says ccrx is running as {pid}, but health check failed at {endpoint}; run `ccrx restart` or inspect {}",
                paths.log_file.display()
            );
        }
        remove_pid_file(paths)?;
    }

    fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("failed to create {}", paths.state_dir.display()))?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)
        .with_context(|| format!("failed to open {}", paths.log_file.display()))?;
    let log_for_stderr = log
        .try_clone()
        .with_context(|| format!("failed to clone {}", paths.log_file.display()))?;

    let exe = env::current_exe().context("failed to locate current executable")?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_for_stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let child = command.spawn().context("failed to start ccrx service")?;
    let pid = child.id();
    fs::write(&paths.pid_file, format!("{pid}\n"))
        .with_context(|| format!("failed to write {}", paths.pid_file.display()))?;

    for _ in 0..50 {
        if health_check(config) {
            println!("started ccrx at {endpoint} (pid {pid})");
            println!("log: {}", paths.log_file.display());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    bail!(
        "started process {pid}, but health check did not pass at {endpoint}; inspect {}",
        paths.log_file.display()
    );
}

fn stop_service_with_paths(config: &AppConfig, paths: &ServicePaths) -> Result<()> {
    let Some(pid) = read_pid(paths)? else {
        if health_check(config) {
            bail!(
                "ccrx is reachable at {}, but no pid file exists at {}; stop the owning process manually",
                endpoint(config),
                paths.pid_file.display()
            );
        }
        println!("ccrx is already stopped");
        return Ok(());
    };

    if !process_exists(pid) {
        remove_pid_file(paths)?;
        println!("ccrx was not running; removed stale pid file {pid}");
        return Ok(());
    }

    send_signal(pid, "TERM")?;
    for _ in 0..50 {
        if !process_exists(pid) && !health_check(config) {
            remove_pid_file(paths)?;
            println!("stopped ccrx (pid {pid})");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    send_signal(pid, "KILL")?;
    remove_pid_file(paths)?;
    println!("stopped ccrx (pid {pid}) with SIGKILL");
    Ok(())
}

fn endpoint(config: &AppConfig) -> String {
    format!("http://{}:{}", config.host, config.port)
}

fn health_check(config: &AppConfig) -> bool {
    let Ok(mut addrs) = (config.host.as_str(), config.port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return false;
    }
    if stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return false;
    }

    let request = format!(
        "GET /health HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        config.host, config.port
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return false;
    }

    response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200")
}

fn read_pid(paths: &ServicePaths) -> Result<Option<u32>> {
    let text = match fs::read_to_string(&paths.pid_file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", paths.pid_file.display()))
        }
    };

    let pid = text
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid pid file {}", paths.pid_file.display()))?;
    Ok(Some(pid))
}

fn remove_pid_file(paths: &ServicePaths) -> Result<()> {
    match fs::remove_file(&paths.pid_file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove {}", paths.pid_file.display()))
        }
    }
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn send_signal(pid: u32, signal: &str) -> Result<()> {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to send SIG{signal} to {pid}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("failed to send SIG{signal} to {pid}"))
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_paths_are_named_for_the_router_state() {
        let paths = ServicePaths::from_state_dir(PathBuf::from("/tmp/example-state"));

        assert_eq!(paths.state_dir, PathBuf::from("/tmp/example-state"));
        assert_eq!(
            paths.pid_file,
            PathBuf::from("/tmp/example-state/codex-code-router.pid")
        );
        assert_eq!(
            paths.log_file,
            PathBuf::from("/tmp/example-state/codex-code-router.log")
        );
    }
}
