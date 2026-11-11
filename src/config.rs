//! Settings: CLI flag > environment variable > config file > built-in default.
//! Plus a small state file recording what was last flashed to each board.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{env, fs, io};

pub const TAILSCALE_PI: &str = "ectf@100.92.231.53";
pub const TAILSCALE_DASHBOARD: &str = "http://100.92.231.53:8080";
pub const LAN_PI: &str = "ectf@10.37.1.253";
pub const LAN_DASHBOARD: &str = "http://10.37.1.253:8080";
pub const DEFAULT_BOARD: &str = "board1";
pub const DEFAULT_REMOTE_SCRIPT: &str = "~/ectf2027/ectf.sh";

/// On-disk config (config.toml). Every field is optional.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gdb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
}

/// Final settings used by the commands.
#[derive(Debug, Clone)]
pub struct Config {
    pub user: Option<String>,
    pub pi: String,
    pub dashboard: String,
    pub board: String,
    /// Put into the remote command as-is (so `~` expands on the Pi).
    pub remote_script: String,
    pub gdb: Option<PathBuf>,
    pub ssh: String,
    pub force: bool,
}

fn env_var(key: &str) -> Option<String> {
    env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl Config {
    pub fn load(board_flag: Option<String>) -> Result<Self> {
        let file = load_file()?;
        let pick = |key: &str, from_file: Option<String>, default: &str| {
            env_var(key).or(from_file).unwrap_or_else(|| default.to_string())
        };
        let board = board_flag
            .or_else(|| env_var("ECTF_BOARD"))
            .or(file.board.clone())
            .unwrap_or_else(|| DEFAULT_BOARD.to_string());
        validate_board(&board)?;
        Ok(Self {
            user: env_var("ECTF_USER")
                .or(file.user.clone())
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty()),
            pi: pick("ECTF_PI", file.pi.clone(), TAILSCALE_PI),
            dashboard: pick("ECTF_DASHBOARD", file.dashboard.clone(), TAILSCALE_DASHBOARD)
                .trim_end_matches('/')
                .to_string(),
            board,
            remote_script: pick("ECTF_REMOTE_SCRIPT", file.remote_script.clone(), DEFAULT_REMOTE_SCRIPT),
            gdb: env_var("ECTF_GDB").or(file.gdb.clone()).map(PathBuf::from),
            ssh: pick("ECTF_SSH", file.ssh.clone(), "ssh"),
            force: env_var("ECTF_FORCE").is_some(),
        })
    }

    /// The name shown on the dashboard. Required for anything that claims or
    /// uses a board, so claims and flashes always carry the same name.
    pub fn user(&self) -> Result<&str> {
        self.user
            .as_deref()
            .context("no user name configured: run `ectf setup` (or set ECTF_USER)")
    }
}

pub fn validate_board(b: &str) -> Result<()> {
    if b.is_empty() || !b.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid board name {b:?} (expected something like board1)");
    }
    Ok(())
}

pub fn valid_user(u: &str) -> bool {
    let n = u.chars().count();
    (1..=32).contains(&n) && u.chars().all(|c| c.is_alphanumeric() || " ._-".contains(c))
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = env::var_os("ECTF_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    Ok(dirs::config_dir()
        .context("no config directory on this system (set ECTF_CONFIG)")?
        .join("ectf")
        .join("config.toml"))
}

pub fn load_file() -> Result<FileConfig> {
    let path = config_path()?;
    match fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save_file(c: &FileConfig) -> Result<PathBuf> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&path, toml::to_string_pretty(c)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// What this machine last flashed to each board (used by `ectf debug`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub last_flash: BTreeMap<String, FlashRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashRecord {
    pub elf: PathBuf,
    pub sha256: String,
    pub unix_time: u64,
}

fn state_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("ectf").join("state.json"))
}

impl State {
    pub fn load() -> Self {
        state_path()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let p = state_path().context("no data directory on this system")?;
        if let Some(d) = p.parent() {
            fs::create_dir_all(d)?;
        }
        fs::write(&p, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}
