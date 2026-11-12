//! `ectf setup` (interactive, run once) and `ectf doctor` (checks).

use crate::config::{self, Config};
use crate::ssh::{spawn_error, Ssh};
use crate::{boards, debug, flash};
use anyhow::{bail, Context, Result};
use std::{error, fs};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn prompt(q: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) if !d.is_empty() => print!("{q} [{d}]: "),
        _ => print!("{q}: "),
    }
    io::stdout().flush()?;
    let mut s = String::new();
    if io::stdin().read_line(&mut s)? == 0 {
        bail!("aborted (no input)");
    }
    let s = s.trim();
    Ok(if s.is_empty() { default.unwrap_or("").to_string() } else { s.to_string() })
}

pub fn setup() -> Result<()> {
    let mut f = config::load_file()?;
    println!("ectf setup (config: {})\n", config::config_path()?.display());

    let user = loop {
        let u = prompt("Your name for the dashboard (use the same one on every machine)", f.user.as_deref())?;
        if config::valid_user(&u) {
            break u;
        }
        println!("  1-32 characters: letters, digits, space, '.', '_', '-'");
    };
    f.user = Some(user);

    let current = if f.pi.as_deref() == Some(config::LAN_PI) { "lan" } else { "tailscale" };
    let (pi, dash) = loop {
        match prompt("Reach the Pi via `tailscale` or `lan`", Some(current))?.to_ascii_lowercase().as_str() {
            "tailscale" | "ts" => break (config::TAILSCALE_PI, config::TAILSCALE_DASHBOARD),
            "lan" => break (config::LAN_PI, config::LAN_DASHBOARD),
            _ => println!("  answer tailscale or lan"),
        }
    };
    f.pi = Some(pi.into());
    f.dashboard = Some(dash.into());

    let board = loop {
        let b = prompt("Default board", Some(f.board.as_deref().unwrap_or(config::DEFAULT_BOARD)))?;
        if config::validate_board(&b).is_ok() {
            break b;
        }
        println!("  e.g. board1");
    };
    f.board = Some(board);

    println!("saved {}\n", config::save_file(&f)?.display());

    let cfg = Config::load(None)?;
    ssh_key_setup(&cfg)?;

    match debug::find_gdb(&cfg) {
        Ok(p) => println!("GDB: {}", p.display()),
        Err(e) => println!("GDB: not found (only needed for `ectf debug`)\n{e:#}"),
    }
    match boards::api(&cfg, "/api/status", None) {
        Ok(_) => println!("dashboard: ok ({})", cfg.dashboard),
        Err(e) => println!("dashboard: {e:#}"),
    }
    println!("\nDone. Try `ectf status`.");
    Ok(())
}

pub fn ssh_ok(cfg: &Config) -> bool {
    Command::new(&cfg.ssh)
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "StrictHostKeyChecking=accept-new"])
        .arg(&cfg.pi)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn find_pubkey() -> Option<PathBuf> {
    let ssh_dir = dirs::home_dir()?.join(".ssh");
    ["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"]
        .iter()
        .map(|n| ssh_dir.join(n))
        .find(|p| p.is_file())
}

/// Replaces ssh-copy-id, which Windows doesn't have.
fn ssh_key_setup(cfg: &Config) -> Result<()> {
    print!("SSH to {} ... ", cfg.pi);
    io::stdout().flush()?;
    if ssh_ok(cfg) {
        println!("ok (key auth works)");
        return Ok(());
    }
    println!("key auth not working yet");

    let key = match find_pubkey() {
        Some(k) => k,
        None => {
            if !prompt("No SSH key found. Create one now? (y/n)", Some("y"))?.starts_with(['y', 'Y']) {
                bail!("an SSH key is required; create one with `ssh-keygen -t ed25519`");
            }
            let path = dirs::home_dir().context("no home directory")?.join(".ssh").join("id_ed25519");
            fs::create_dir_all(path.parent().unwrap())?;
            let st = Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-f"])
                .arg(&path)
                .status()
                .map_err(|e| spawn_error("ssh-keygen", e))?;
            if !st.success() {
                bail!("ssh-keygen failed");
            }
            path.with_extension("pub")
        }
    };

    println!("Installing {} on {} (enter the Pi's password when asked)", key.display(), cfg.pi);
    let pubkey = fs::read_to_string(&key)?;
    let mut child = Command::new(&cfg.ssh)
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .arg(&cfg.pi)
        .arg("umask 077; mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_error(&cfg.ssh, e))?;
    child.stdin.take().unwrap().write_all(format!("{}\n", pubkey.trim()).as_bytes())?;
    if !child.wait()?.success() {
        bail!("installing the key failed");
    }

    if ssh_ok(cfg) {
        println!("SSH: ok");
    } else {
        println!(
            "SSH: key installed, but non-interactive login still fails.\n\
             If your key has a passphrase, load it into ssh-agent (Windows: start the\n\
             \"OpenSSH Authentication Agent\" service, then `ssh-add`)."
        );
    }
    Ok(())
}

pub fn doctor(cfg: &Config) -> Result<i32> {
    println!("ectf {}", env!("CARGO_PKG_VERSION"));
    println!("config:        {}", config::config_path()?.display());
    println!("user:          {}", cfg.user.as_deref().unwrap_or("(not set: run `ectf setup`)"));
    println!("pi:            {}", cfg.pi);
    println!("dashboard:     {}", cfg.dashboard);
    println!("board:         {}", cfg.board);
    println!("remote script: {}\n", cfg.remote_script);

    let mut ok = cfg.user.is_some();
    let mut report = |what: &str, good: bool, hint: &str| {
        println!("{}  {what}{}", if good { "ok  " } else { "FAIL" }, if good { String::new() } else { format!(": {hint}") });
        ok &= good;
    };

    let ssh_good = ssh_ok(cfg);
    report("ssh key auth", ssh_good, "run `ectf setup`; if your key has a passphrase, load it into ssh-agent");
    if ssh_good {
        let r = Ssh::new(cfg).run_capture(&format!("{} {} gdbport", cfg.remote_script, cfg.board));
        report("ectf.sh on the Pi", r.is_ok(), "check ECTF_REMOTE_SCRIPT / the board name");
    }
    let dash = boards::api(cfg, "/api/status", None);
    report("dashboard", dash.is_ok(), "check Tailscale/LAN and ECTF_DASHBOARD");
    match flash::find_objcopy() {
        Some(p) => report(&format!("objcopy: {}", p.display()), true, ""),
        None => report("objcopy", false, "run `rustup component add llvm-tools` (or set ECTF_OBJCOPY)"),
    }
    match debug::find_gdb(cfg) {
        Ok(p) => println!("ok    gdb: {}", p.display()),
        Err(e) => println!("warn  gdb (only needed for `ectf debug`): {e:#}"),
    }
    Ok(if ok { 0 } else { 1 })
}
