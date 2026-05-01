use std::env;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DnsStrategy {
    Virtual,
    OverTcp,
    Direct,
}

impl DnsStrategy {
    pub fn as_tun2proxy_value(self) -> &'static str {
        match self {
            DnsStrategy::Virtual => "virtual",
            DnsStrategy::OverTcp => "over-tcp",
            DnsStrategy::Direct => "direct",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DnsStrategy::Virtual => "virtual",
            DnsStrategy::OverTcp => "over-tcp",
            DnsStrategy::Direct => "direct",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpnConfig {
    pub tun2proxy_path: Option<PathBuf>,
    pub setup_routes: bool,
    pub bypass: Vec<String>,
    pub dns_strategy: DnsStrategy,
    pub enable_ipv6: bool,
}

impl Default for VpnConfig {
    fn default() -> Self {
        Self {
            tun2proxy_path: None,
            setup_routes: true,
            bypass: vec!["127.0.0.0/8".to_owned(), "::1/128".to_owned()],
            dns_strategy: DnsStrategy::Virtual,
            enable_ipv6: false,
        }
    }
}

pub struct VpnProcess {
    child: Child,
}

impl VpnProcess {
    pub fn start(proxy_port: u16, config: VpnConfig, log: mpsc::Sender<String>) -> Result<Self> {
        let executable = resolve_tun2proxy(config.tun2proxy_path.as_deref())
            .context("tun2proxy-bin was not found")?;

        let mut command = Command::new(&executable);
        command
            .arg("--proxy")
            .arg(format!("socks5://127.0.0.1:{proxy_port}"))
            .arg("--dns")
            .arg(config.dns_strategy.as_tun2proxy_value())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if config.setup_routes {
            command.arg("--setup");
        }
        if config.enable_ipv6 {
            command.arg("--ipv6-enabled");
        }
        for bypass in config.bypass.iter().filter(|item| !item.trim().is_empty()) {
            command.arg("--bypass").arg(bypass.trim());
        }

        let _ = log.send(format!(
            "starting TUN sidecar: {} {}",
            executable.display(),
            command_args_for_log(&command).join(" ")
        ));

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", executable.display()))?;

        if let Some(stdout) = child.stdout.take() {
            spawn_pipe_reader(stdout, "tun2proxy", log.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_pipe_reader(stderr, "tun2proxy", log.clone());
        }

        let _ = log.send(format!("started TUN sidecar: {}", executable.display()));
        Ok(Self { child })
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child.try_wait().context("failed to poll tun2proxy")
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.kill().context("failed to stop tun2proxy")?;
            let _ = self.child.wait();
        }
        Ok(())
    }
}

fn command_args_for_log(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| quote_arg_for_log(&arg.to_string_lossy()))
        .collect()
}

fn quote_arg_for_log(value: &str) -> String {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

impl Drop for VpnProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub fn resolve_tun2proxy(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(anyhow!(
            "configured tun2proxy path does not exist: {}",
            path.display()
        ));
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            candidates.extend(candidate_names().into_iter().map(|name| dir.join(name)));
            candidates.extend(
                candidate_names()
                    .into_iter()
                    .map(|name| dir.join("bin").join(name)),
            );
        }
    }

    for path in candidates {
        if path.exists() {
            return Ok(path);
        }
    }

    find_on_path().ok_or_else(|| {
        anyhow!(
            "tun2proxy-bin not found beside net-combiner or on PATH; place tun2proxy-bin next to the app or set an explicit path"
        )
    })
}

fn candidate_names() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["tun2proxy-bin.exe", "tun2proxy.exe"]
    } else {
        vec!["tun2proxy-bin", "tun2proxy"]
    }
}

fn find_on_path() -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    let extensions = path_extensions();
    for dir in env::split_paths(&path_var) {
        for name in candidate_names() {
            let base = dir.join(name);
            if base.exists() {
                return Some(base);
            }
            if cfg!(windows) && Path::new(name).extension().is_none() {
                for extension in &extensions {
                    let mut value = OsString::from(name);
                    value.push(extension);
                    let path = dir.join(value);
                    if path.exists() {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

fn path_extensions() -> Vec<OsString> {
    env::var_os("PATHEXT")
        .map(|value| {
            env::split_paths(&value)
                .map(|path| path.as_os_str().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn spawn_pipe_reader<R>(reader: R, prefix: &'static str, log: mpsc::Sender<String>)
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let _ = log.send(format!("{prefix}: {line}"));
                }
                Err(error) => {
                    let _ = log.send(format!("{prefix}: failed to read output: {error}"));
                    break;
                }
            }
        }
    });
}
