//! Dashboard API: status, claim, release.

use crate::config::{validate_board, Config};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub fn api(cfg: &Config, path: &str, body: Option<Value>) -> Result<Value> {
    let url = format!("{}{path}", cfg.dashboard);
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
    let res = match body {
        Some(b) => agent.post(&url).send_json(b),
        None => agent.get(&url).call(),
    };
    match res {
        Ok(r) => r.into_json().context("dashboard returned invalid JSON"),
        Err(ureq::Error::Status(code, r)) => {
            let msg = r
                .into_json::<Value>()
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("HTTP {code}"));
            bail!("{msg}")
        }
        Err(e) => bail!(
            "can't reach {} ({e}); is Tailscale/LAN up? (set ECTF_DASHBOARD or rerun `ectf setup`)",
            cfg.dashboard
        ),
    }
}

pub fn status(cfg: &Config, json_out: bool) -> Result<()> {
    let st = api(cfg, "/api/status", None)?;
    // Optional: does nothing until the Pi adds "min_client" to /api/status.
    if let Some(min) = st["min_client"].as_str() {
        let me = env!("CARGO_PKG_VERSION");
        if version_lt(me, min) {
            eprintln!("warning: ectf {me} is older than the Pi expects ({min}); update it (see README)");
        }
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&st)?);
    } else {
        show(&st, cfg.user.as_deref());
    }
    Ok(())
}

pub fn claim(cfg: &Config, board: Option<&str>, dur: &str, note: &str) -> Result<()> {
    let board = board.unwrap_or(&cfg.board);
    validate_board(board)?;
    let user = cfg.user()?;
    let r = api(cfg, "/api/claim", Some(json!({ "board": board, "name": user, "duration": dur, "note": note })))?;
    let c = &r["claim"];
    println!(
        "{board} claimed by {} until {}",
        c["name"].as_str().unwrap_or(user),
        hm(c["until"].as_f64().unwrap_or(0.0))
    );
    Ok(())
}

pub fn release(cfg: &Config, board: Option<&str>, force: bool) -> Result<()> {
    let board = board.unwrap_or(&cfg.board);
    validate_board(board)?;
    let user = cfg.user()?;
    api(cfg, "/api/release", Some(json!({ "board": board, "name": user, "force": force })))?;
    println!("{board} released");
    Ok(())
}

fn show(st: &Value, me: Option<&str>) {
    let now = st["now"].as_f64().unwrap_or(0.0);
    println!("{:<8} {:<32} {:<26} {:<18} GDB", "BOARD", "CLAIM", "ACTIVITY", "BOOTLOADER");
    for b in st["boards"].as_array().into_iter().flatten() {
        let (c, a, bl) = (&b["claim"], &b["activity"], &b["bootloader"]);
        let claim = if non_empty_obj(c) {
            let name = c["name"].as_str().unwrap_or("?");
            let you = me.is_some_and(|m| m.to_lowercase() == name.to_lowercase());
            format!("{name} until {}{}", hm(c["until"].as_f64().unwrap_or(0.0)), if you { " (you)" } else { "" })
        } else {
            "free".into()
        };
        let activity = if non_empty_obj(a) {
            format!(
                "{} by {} {}",
                a["action"].as_str().unwrap_or("?"),
                a["user"].as_str().unwrap_or("?"),
                ago(a["since"].as_f64().unwrap_or(now), now)
            )
        } else if !b["port_present"].as_bool().unwrap_or(true) {
            "PROBE MISSING".into()
        } else {
            "idle".into()
        };
        let boot = if non_empty_obj(bl) {
            let secure = bl["secure"].as_bool() == Some(true) || bl["secure"].as_str() == Some("True");
            format!("{} {}", display(&bl["version"]), if secure { "SECURE" } else { "insecure" })
        } else {
            "?".into()
        };
        let gdb = match &b["gdb_port"] {
            Value::Null | Value::Bool(false) => "-".into(),
            Value::String(s) if s.is_empty() => "-".into(),
            Value::Number(n) if n.as_u64() == Some(0) => "-".into(),
            v => display(v),
        };
        println!(
            "{:<8} {:<32} {:<26} {:<18} {gdb}",
            display(&b["name"]),
            trunc(&claim, 32),
            trunc(&activity, 26),
            boot
        );
    }
}

fn non_empty_obj(v: &Value) -> bool {
    v.as_object().is_some_and(|o| !o.is_empty())
}

fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "?".into(),
        v => v.to_string(),
    }
}

fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn hm(t: f64) -> String {
    chrono::DateTime::from_timestamp(t as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

fn ago(t: f64, now: f64) -> String {
    let s = (now - t).max(0.0) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    }
}

fn version_lt(a: &str, b: &str) -> bool {
    let parse = |s: &str| {
        let mut v: Vec<u64> = s.split('.').map(|x| x.trim().parse().unwrap_or(0)).collect();
        v.resize(3, 0);
        v
    };
    parse(a) < parse(b)
}
