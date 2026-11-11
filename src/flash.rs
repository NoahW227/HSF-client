//! `ectf flash <elf>`: the cargo runner.

use crate::config::{Config, FlashRecord, State};
use crate::ssh::{self, quote, Ssh};
use anyhow::{anyhow, bail, Context, Result};
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
    let bin = elf_to_bin(&elf).with_context(|| format!("converting {}", elf_path.display()))?;

    let bin_path = with_suffix(&elf_path, ".bin");
    fs::write(&bin_path, &bin).with_context(|| format!("writing {}", bin_path.display()))?;
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
            FlashRecord { elf: elf_path.clone(), sha256: sha, unix_time: unix_now().0 },
        );
        if let Err(e) = state.save() {
            eprintln!("warning: couldn't record flash for `ectf debug`: {e:#}");
        }
    }
    Ok(status.code().unwrap_or(1))
}

/// Same output as `objcopy -O binary`: the file contents of every PT_LOAD
/// segment, placed at its physical (load) address, with gaps zero-filled.
pub fn elf_to_bin(elf: &[u8]) -> Result<Vec<u8>> {
    use object::elf::{FileHeader32, EM_ARM, PT_LOAD};
    use object::read::elf::{FileHeader, ProgramHeader};
    use object::Endianness;

    let hdr = FileHeader32::<Endianness>::parse(elf).context("not a 32-bit ELF file")?;
    let endian = hdr.endian().context("bad ELF endianness")?;
    if hdr.e_machine(endian) != EM_ARM {
        bail!("not an ARM ELF (is the build target thumbv6m-none-eabi?)");
    }
    let mut segs: Vec<(u64, &[u8])> = Vec::new();
    for ph in hdr.program_headers(endian, elf).context("bad ELF program headers")? {
        if ph.p_type(endian) != PT_LOAD || ph.p_filesz(endian) == 0 {
            continue;
        }
        let data = ph.data(endian, elf).map_err(|_| anyhow!("ELF segment out of bounds"))?;
        segs.push((u64::from(ph.p_paddr(endian)), data));
    }
    let start = segs.iter().map(|s| s.0).min().context("ELF has no loadable data")?;
    let end = segs.iter().map(|s| s.0 + s.1.len() as u64).max().unwrap();
    if end - start > MAX_IMAGE {
        bail!(
            "image would span {:#x}..{:#x} ({} bytes); check the linker script's load addresses",
            start, end, end - start
        );
    }
    let mut out = vec![0u8; (end - start) as usize];
    for (addr, data) in segs {
        let off = (addr - start) as usize;
        out[off..off + data.len()].copy_from_slice(data);
    }
    Ok(out)
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
