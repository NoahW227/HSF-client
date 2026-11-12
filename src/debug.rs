//! `ectf debug`: OpenOCD on the Pi, GDB on this machine, an SSH tunnel between them.

use crate::config::{Config, State};
use crate::flash;
use crate::ssh::{self, quote, Ssh};
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{env, process, thread};

const READY: &str = "ectf: gdb server ready";
const READY_TIMEOUT: Duration = Duration::from_secs(20);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const GDB_HINT: &str = "no ARM-capable GDB found. Install one:
  Debian/Ubuntu: sudo apt install gdb-multiarch
  Arch:          sudo pacman -S arm-none-eabi-gdb
  Fedora:        sudo dnf install gdb
  Windows:       Arm GNU Toolchain (arm-none-eabi), with its bin\\ on PATH
or set ECTF_GDB (or `gdb = \"...\"` in the config) to its path";

pub fn run(cfg: &Config, elf: Option<PathBuf>, local_port: Option<u16>, gdb_args: &[String]) -> Result<i32> {
    let user = cfg.user()?;
    let elf = resolve_elf(cfg, elf)?;
    let gdb = find_gdb(cfg)?; // check before starting anything on the Pi
    warn_if_not_flashed(cfg, &elf);

    // Ctrl-C, SIGTERM and SIGHUP never kill this process outright, so the
    // tunnel cleanup always runs. While we wait for OpenOCD they abort the
    // wait; while GDB runs they're ignored here (GDB uses Ctrl-C to interrupt the target).
    ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst))
        .context("installing Ctrl-C handler")?;

    let ssh = Ssh::new(cfg);
    let port: u16 = ssh
        .run_capture(&format!("{} {} gdbport", cfg.remote_script, cfg.board))?
        .trim()
        .parse()
        .context("the Pi returned an invalid GDB port")?;
    let local = match local_port {
        Some(p) => p,
        None => match env::var("ECTF_LOCAL_PORT") {
            Ok(v) if !v.trim().is_empty() => v.trim().parse().context("ECTF_LOCAL_PORT is not a port number")?,
            _ => port,
        },
    };

    let mut tunnel = Tunnel::start(cfg, &ssh, user, port, local)?;
    tunnel.wait_ready(local)?;
    println!("{}: attached via 127.0.0.1:{local} -> {}", cfg.board, cfg.pi);

    let status = Command::new(&gdb)
        .arg("-q")
        .arg(&elf)
        .arg("-ex")
        .arg(format!("target extended-remote 127.0.0.1:{local}"))
        .args(gdb_args)
        .status()
        .with_context(|| format!("running {}", gdb.display()))?;
    drop(tunnel); // stops OpenOCD on the Pi and releases the board
    Ok(status.code().unwrap_or(1))
}

fn resolve_elf(cfg: &Config, elf: Option<PathBuf>) -> Result<PathBuf> {
    let path = match elf {
        Some(p) => flash::absolute(&p)?,
        None => State::load()
            .last_flash
            .get(&cfg.board)
            .map(|r| r.elf.clone())
            .with_context(|| {
                format!(
                    "no ELF given and nothing flashed to {} from this machine yet\n\
                     usage: ectf debug <elf> [-- <gdb args>]",
                    cfg.board
                )
            })?,
    };
    if !path.is_file() {
        bail!("ELF not found: {}", path.display());
    }
    Ok(path)
}

/// GDB needs the exact image that's on the board. This can only check what
/// *this machine* flashed, not flashes by other people.
fn warn_if_not_flashed(cfg: &Config, elf: &Path) {
    let state = State::load();
    let Some(rec) = state.last_flash.get(&cfg.board) else {
        eprintln!(
            "note: no record of flashing {} from this machine; make sure {} is what's on it",
            cfg.board,
            elf.display()
        );
        return;
    };
    let sha = fs::read(elf).ok().map(|d| flash::sha256_hex(&d));
    if sha.as_deref() != Some(rec.elf_sha256.as_str()) {
        eprintln!(
            "warning: {} is not the image you last flashed to {} (rebuilt without flashing?). \
             Symbols may not match; run `cargo run --release` first.",
            elf.display(),
            cfg.board
        );
    }
}

pub fn find_gdb(cfg: &Config) -> Result<PathBuf> {
    if let Some(g) = &cfg.gdb {
        return Ok(g.clone());
    }
    for name in ["gdb-multiarch", "arm-none-eabi-gdb", "gdb"] {
        if let Some(p) = which(name) {
            if gdb_supports_arm(&p) {
                return Ok(p);
            }
        }
    }
    bail!("{GDB_HINT}")
}

fn gdb_supports_arm(gdb: &Path) -> bool {
    Command::new(gdb)
        .args(["-q", "-nx", "-batch", "-ex", "set architecture armv6-m"])
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stderr).contains("Undefined"))
        .unwrap_or(false)
}

pub fn which(name: &str) -> Option<PathBuf> {
    let exe = format!("{name}{}", env::consts::EXE_SUFFIX);
    env::split_paths(&env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// The `ssh -tt -L` process running `ectf.sh <board> debug` on the Pi.
/// Dropping it kills ssh. The remote side then gets a hangup and OpenOCD exits.
struct Tunnel {
    child: Child,
    log: PathBuf,
}

impl Tunnel {
    fn start(cfg: &Config, ssh: &Ssh, user: &str, port: u16, local: u16) -> Result<Self> {
        let log = env::temp_dir().join(format!("ectf-debug-{}.log", process::id()));
        let f = File::create(&log).with_context(|| format!("creating {}", log.display()))?;

        let mut remote = format!("ECTF_USER={} ", quote(user));
        if cfg.force {
            remote.push_str("ECTF_FORCE=1 ");
        }
        remote.push_str(&format!("{} {} debug", cfg.remote_script, cfg.board));

        let mut c = ssh.base();
        c.args(["-tt", "-o", "ExitOnForwardFailure=yes", "-L"])
            .arg(format!("{local}:127.0.0.1:{port}"))
            .arg(&cfg.pi)
            .arg(remote)
            .stdin(Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f);
        detach(&mut c);
        let child = c.spawn().map_err(|e| ssh::spawn_error(&cfg.ssh, e))?;
        Ok(Self { child, log })
    }

    fn wait_ready(&mut self, local: u16) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let log = String::from_utf8_lossy(&fs::read(&self.log).unwrap_or_default()).into_owned();
            if log.contains(READY) {
                return Ok(());
            }
            let failed = log.lines().any(|l| l.trim_start().starts_with("error:"));
            if failed || self.child.try_wait()?.is_some() {
                if log.contains("Address already in use") {
                    bail!("local port {local} is busy; use --local-port <N> (or ECTF_LOCAL_PORT)");
                }
                let errs: Vec<&str> = log
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.starts_with("error:") || l.contains("Error:"))
                    .collect();
                bail!(
                    "debug session failed to start:\n{}",
                    if errs.is_empty() { log.trim().to_string() } else { errs.join("\n") }
                );
            }
            if INTERRUPTED.load(Ordering::SeqCst) {
                bail!("interrupted");
            }
            if Instant::now() >= deadline {
                bail!("OpenOCD did not come up within {}s:\n{}", READY_TIMEOUT.as_secs(), log.trim());
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.log);
    }
}

/// Start ssh outside the terminal's Ctrl-C group, so Ctrl-C in GDB interrupts
/// the target instead of killing the tunnel.
#[cfg(unix)]
fn detach(c: &mut Command) {
    use std::os::unix::process::CommandExt;
    c.process_group(0);
}

#[cfg(windows)]
fn detach(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200; // also disables Ctrl-C for the child
    c.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_: &mut Command) {}
