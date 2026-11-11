//! Runs the system `ssh`, so users keep their existing keys, agent and ~/.ssh/config.

use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Quote a value for the POSIX shell on the Pi.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./_-".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn spawn_error(bin: &str, e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::NotFound {
        anyhow!(
            "`{bin}` not found. Linux: install your distro's OpenSSH client. \
             Windows: Settings > System > Optional features > add \"OpenSSH Client\"."
        )
    } else {
        anyhow!("running `{bin}`: {e}")
    }
}

pub struct Ssh<'a> {
    cfg: &'a Config,
}

impl<'a> Ssh<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Self { cfg }
    }

    /// `ssh` with BatchMode: it never prompts, so it fails quickly instead of
    /// hanging when key auth isn't set up. The host is added by the caller.
    pub fn base(&self) -> Command {
        let mut c = Command::new(&self.cfg.ssh);
        c.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"]);
        c
    }

    /// Run a remote command and return its stdout. Its stderr goes to the user.
    pub fn run_capture(&self, remote: &str) -> Result<String> {
        let out = self
            .base()
            .arg(&self.cfg.pi)
            .arg(remote)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| spawn_error(&self.cfg.ssh, e))?;
        if !out.status.success() {
            bail!("`{remote}` failed on {} ({})", self.cfg.pi, out.status);
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Upload bytes with `cat > path`. This avoids scp, which reads
    /// `C:\...` as a host name.
    pub fn upload(&self, data: &[u8], remote_path: &str) -> Result<()> {
        let mut child = self
            .base()
            .arg(&self.cfg.pi)
            .arg(format!("cat > {}", quote(remote_path)))
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| spawn_error(&self.cfg.ssh, e))?;
        let written = child.stdin.take().expect("piped stdin").write_all(data);
        let status = child.wait()?;
        if !status.success() {
            bail!("upload to {}:{remote_path} failed ({status})", self.cfg.pi);
        }
        written.context("upload: writing to ssh")?;
        Ok(())
    }
}
