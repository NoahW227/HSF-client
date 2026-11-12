//! `ectf flash <elf>`: the cargo runner.

use crate::config::{Config, FlashRecord, State};
use crate::ssh::{self, quote, Ssh};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, io, process};

/// Sanity limit: much bigger than the MSPM0L2228's flash. An image this large
/// usually means a segment has a RAM load address.
const MAX_IMAGE: u64 = 1 << 20;

pub fn run(cfg: &Config, elf_path: &Path, extra: &[String]) -> Result<i32> {
    if !extra.is_empty() {
        eprintln!("note: ignoring extra arguments {extra:?}");
    }
    let user = cfg.user()?;
    let elf_path = absolute(elf_path)?;
    let elf = fs::read(&elf_path).with_context(|| format!("reading {}", elf_path.display()))?;
    let elf_sha = sha256_hex(&elf);
    
    let bin_path = with_suffix(&elf_path, ".bin");
    objcopy(&elf_path, &bin_path)?;
    let bin = fs::read(&bin_path).with_context(|| format!("reading {}", bin_path.display()))?;
    if bin.is_empty() || bin.starts_with(b"\x7fELF") {
        bail!("{} doesn't look like a raw image", bin_path.display());
    }
    let sha = sha256_hex(&bin);
    println!("built {} ({} bytes) -> {} {}", bin_path.display(), bin.len(), cfg.pi, cfg.board);

    let label = env::var("ECTF_IMAGE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| image_label(&elf_path));
    let image = format!("{} sha256:{}", git_describe(&elf_path), &sha[..12]);

    // Unique name so two people flashing the same board can't overwrite each
    // other's upload. The board lock on the Pi then serializes the flash itself.
    let remote_bin = format!(
        "/tmp/ectf-{}-{}-{}-{}.bin",
        sanitize(user),
        cfg.board,
        process::id(),
        unix_now().1
    );
    let ssh = Ssh::new(cfg);
    ssh.upload(&bin, &remote_bin)?;

    let mut remote = format!(
        "ECTF_USER={} ECTF_IMAGE={} ECTF_IMAGE_NAME={} ",
        quote(user),
        quote(&image),
        quote(&label)
    );
    if cfg.force {
        remote.push_str("ECTF_FORCE=1 ");
    }
    remote.push_str(&format!(
        "{} {} flash {rb}; rc=$?; rm -f {rb}; exit $rc",
        cfg.remote_script,
        cfg.board,
        rb = quote(&remote_bin)
    ));

    let mut cmd = ssh.base();
    if io::stdin().is_terminal() {
        cmd.arg("-t");
    }
    let status = cmd
        .arg(&cfg.pi)
        .arg(remote)
        .status()
        .map_err(|e| ssh::spawn_error(&cfg.ssh, e))?;

    if status.success() {
        let mut state = State::load();
        state.last_flash.insert(
            cfg.board.clone(),
            FlashRecord { elf: elf_path.clone(), sha256: sha, elf_sha256: elf_sha, unix_time: unix_now().0 },
        );
        if let Err(e) = state.save() {
            eprintln!("warning: couldn't record flash for `ectf debug`: {e:#}");
        }
    }
    Ok(status.code().unwrap_or(1))
}

/// Convert with objcopy.
fn objcopy(elf: &Path, bin: &Path) -> Result<()> {
    let tool = find_objcopy().context(
        "no objcopy found: run `rustup component add llvm-tools` (or set ECTF_OBJCOPY)",
    )?;
    let out = Command::new(&tool)
        .args(["-O", "binary"])
        .arg(elf)
        .arg(bin)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", tool.display()))?;
    if !out.status.success() {
        bail!("{} failed:\n{}", tool.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Search order: ECTF_OBJCOPY, the rustup llvm-tools component, then rust-objcopy / arm-none-eabi-objcopy on PATH.
pub fn find_objcopy() -> Option<PathBuf> {
    if let Some(p) = env::var_os("ECTF_OBJCOPY").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let rustc = |arg: &[&str]| -> Option<String> {
        let o = Command::new("rustc").args(arg).stderr(Stdio::null()).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if let (Some(sysroot), Some(vv)) = (rustc(&["--print", "sysroot"]), rustc(&["-vV"])) {
        if let Some(host) = vv.lines().find_map(|l| l.strip_prefix("host: ")) {
            let p = PathBuf::from(sysroot.trim())
                .join("lib/rustlib")
                .join(host.trim())
                .join("bin")
                .join(format!("llvm-objcopy{}", env::consts::EXE_SUFFIX));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    ["rust-objcopy", "arm-none-eabi-objcopy"].into_iter().find_map(crate::debug::which)

}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn absolute(p: &Path) -> Result<PathBuf> {
    Ok(if p.is_absolute() { p.to_path_buf() } else { env::current_dir()?.join(p) })
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Bootloader image label: at most 8 characters from [A-Za-z0-9._-].
fn image_label(elf: &Path) -> String {
    elf.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .take(8)
        .collect()
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn git_describe(elf: &Path) -> String {
    let dir = elf.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["describe", "--always", "--dirty"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nogit".into())
}

/// (seconds, nanoseconds part) since the Unix epoch.
fn unix_now() -> (u64, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}
