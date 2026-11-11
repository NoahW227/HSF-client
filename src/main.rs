mod boards;
mod config;
mod debug;
mod flash;
mod setup;
mod ssh;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "ectf", version, about = "Flash, debug and claim eCTF boards on the board Pi")]
struct Cli {
    /// Board to use (default: $ECTF_BOARD, then the config file)
    #[arg(short, long, global = true)]
    board: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show all boards (the default command)
    #[command(alias = "boards")]
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Claim a board for a while
    Claim {
        #[arg(value_name = "BOARD")]
        which: Option<String>,
        /// e.g. 45m, 2h, 1h30m (max 12h)
        #[arg(long = "for", default_value = "2h")]
        dur: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Release a claim
    Release {
        #[arg(value_name = "BOARD")]
        which: Option<String>,
        /// Release someone else's claim
        #[arg(long)]
        force: bool,
    },
    /// Convert an ELF to .bin, upload it and flash it (this is the cargo runner)
    Flash {
        elf: PathBuf,
        /// Override the board lock on the Pi (same as ECTF_FORCE=1)
        #[arg(long)]
        force: bool,
        /// Arguments after `cargo run --`; ignored
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        extra: Vec<String>,
    },
    /// Debug the board with local GDB (default ELF: the last one you flashed to it)
    Debug {
        elf: Option<PathBuf>,
        /// Local tunnel port (default: same as the Pi's port; also ECTF_LOCAL_PORT)
        #[arg(long)]
        local_port: Option<u16>,
        /// Override the board lock on the Pi (same as ECTF_FORCE=1)
        #[arg(long)]
        force: bool,
        /// Extra GDB arguments, e.g. -- -ex 'hbreak main'
        #[arg(last = true)]
        gdb_args: Vec<String>,
    },
    /// Interactive one-time setup: name, network, SSH key
    Setup,
    /// Check config, SSH, dashboard and GDB
    Doctor,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let cmd = cli.cmd.unwrap_or(Cmd::Status { json: false });
    if matches!(cmd, Cmd::Setup) {
        setup::setup()?;
        return Ok(0);
    }
    let mut cfg = config::Config::load(cli.board)?;
    match cmd {
        Cmd::Setup => unreachable!(),
        Cmd::Doctor => setup::doctor(&cfg),
        Cmd::Status { json } => boards::status(&cfg, json).map(|_| 0),
        Cmd::Claim { which, dur, note } => boards::claim(&cfg, which.as_deref(), &dur, &note).map(|_| 0),
        Cmd::Release { which, force } => boards::release(&cfg, which.as_deref(), force).map(|_| 0),
        Cmd::Flash { elf, force, extra } => {
            cfg.force |= force;
            flash::run(&cfg, &elf, &extra)
        }
        Cmd::Debug { elf, local_port, force, gdb_args } => {
            cfg.force |= force;
            debug::run(&cfg, elf, local_port, &gdb_args)
        }
    }
}
