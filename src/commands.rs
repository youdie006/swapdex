//! The subcommand handlers. Each returns an exit code; a hard error propagates
//! and `main` prints a redacted message + exits 1. Output is identity-based and
//! never prints a credential byte (the A11 egress guarantee) - the only reader
//! of a `Secret` is inside the adapters/store.

use crate::adapters::{self, Account, AuthTool};
use crate::paths::Paths;
use crate::store::Store;
use anyhow::Result;
use serde_json::Value;

/// Which tools a command targets. `--tool claude|codex` is explicit; default is
/// "both" (act only on tools that apply).
pub enum ToolSel {
    Claude,
    Codex,
    Both,
}

impl ToolSel {
    pub fn parse(s: Option<&str>) -> ToolSel {
        match s {
            Some("claude") | Some("claude-code") => ToolSel::Claude,
            Some("codex") => ToolSel::Codex,
            _ => ToolSel::Both,
        }
    }
    fn wants(&self, tool: &str) -> bool {
        match self {
            ToolSel::Claude => tool == "claude-code",
            ToolSel::Codex => tool == "codex",
            ToolSel::Both => true,
        }
    }
    fn explicit(&self) -> bool {
        !matches!(self, ToolSel::Both)
    }
}

fn adapters_for(sel: &ToolSel) -> Vec<Box<dyn AuthTool>> {
    adapters::all()
        .into_iter()
        .filter(|a| sel.wants(a.name()))
        .collect()
}

/// The account_id a stored profile's snapshot resolves to, for matching a live
/// identity back to a profile name (A2). Reads the snapshot, not `active.json`.
fn profile_account_id(store: &Store, name: &str, tool: &str) -> Option<String> {
    let snap = store.load(name, tool).ok()??;
    match tool {
        "codex" => {
            let v: Value = serde_json::from_slice(snap.part("auth")?.expose()).ok()?;
            v["tokens"]["account_id"].as_str().map(|s| s.to_string())
        }
        "claude-code" => {
            let v: Value = serde_json::from_slice(snap.part("oauth_account")?.expose()).ok()?;
            v["accountUuid"].as_str().map(|s| s.to_string())
        }
        _ => None,
    }
}

/// Find the stored profile name whose snapshot matches this live account_id.
fn matched_profile(store: &Store, tool: &str, live_id: &str) -> Option<String> {
    if live_id.is_empty() {
        return None;
    }
    store
        .list()
        .into_iter()
        .find(|p| {
            p.tools.iter().any(|t| t == tool)
                && profile_account_id(store, &p.name, tool).as_deref() == Some(live_id)
        })
        .map(|p| p.name)
}

pub fn add(paths: &Paths, name: &str, sel: &ToolSel, update: bool) -> Result<i32> {
    crate::atomic::ensure_not_root()?;
    let store = Store::open(paths)?;
    let mut saved = Vec::new();
    for adapter in adapters_for(sel) {
        let tool = adapter.name();
        if !adapter.present(paths) {
            if sel.explicit() {
                eprintln!("swapdex: not logged in to {tool}");
                return Ok(3);
            }
            continue;
        }
        if store.load(name, tool)?.is_some() && !update {
            eprintln!(
                "swapdex: profile '{name}' already has a {tool} login; pass --update to replace"
            );
            return Ok(6);
        }
        let snap = adapter.capture(paths)?;
        store.save(name, &snap)?;
        saved.push(tool);
    }
    if saved.is_empty() {
        eprintln!("swapdex: not logged in to any selected tool");
        return Ok(3);
    }
    println!("saved profile '{name}' ({})", saved.join(", "));
    Ok(0)
}

pub fn use_account(paths: &Paths, name: &str, sel: &ToolSel, dry_run: bool) -> Result<i32> {
    crate::atomic::ensure_not_root()?;
    let store = Store::open(paths)?;
    let _lock = match store.lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("swapdex: another swapdex is mid-switch; try again");
            return Ok(4);
        }
    };
    let mut switched = 0;
    for adapter in adapters_for(sel) {
        let tool = adapter.name();
        let target = match store.load(name, tool)? {
            Some(s) => s,
            None => {
                if sel.explicit() {
                    eprintln!("swapdex: profile '{name}' has no {tool} login");
                    return Ok(5);
                }
                continue;
            }
        };
        warn_if_expired(&target, tool);
        if dry_run {
            println!("would switch {tool} -> {name}");
            switched += 1;
            continue;
        }
        // Safe order (A6): back up the CURRENT live login first (atomic + fsync
        // inside write_secret); if the backup fails, `?` aborts BEFORE we touch
        // the live login.
        if adapter.present(paths) {
            let live = adapter.capture(paths)?;
            store.backup(&live)?;
        }
        adapter.apply(paths, &target)?;
        store.append_timeline(tool, name, "use")?;
        store.set_active(tool, name)?;
        if let Some(id) = adapter.identity(paths)? {
            println!("switched {tool} -> {}", identity_line(&id));
        }
        switched += 1;
    }
    if switched == 0 {
        eprintln!("swapdex: no profile named '{name}'");
        return Ok(5);
    }
    if !dry_run {
        println!("(takes effect on your next message)");
    }
    Ok(0)
}

pub fn ls(paths: &Paths, json: bool) -> Result<i32> {
    let store = Store::open(paths)?;
    // Active markers come from LIVE identity, matched back to a profile (A2).
    let live: Vec<(String, Account)> = adapters::all()
        .iter()
        .filter_map(|a| {
            a.identity(paths)
                .ok()
                .flatten()
                .map(|id| (a.name().to_string(), id))
        })
        .collect();
    let active_names: Vec<String> = live
        .iter()
        .filter_map(|(tool, id)| matched_profile(&store, tool, &id.account_id))
        .collect();

    let profiles = store.list();
    if json {
        let rows: Vec<Value> = profiles
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "tools": p.tools,
                    "active": active_names.contains(&p.name),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(0);
    }
    if profiles.is_empty() {
        println!("no saved profiles yet - run `swapdex add <name>` while logged in");
        return Ok(0);
    }
    for p in &profiles {
        let mark = if active_names.contains(&p.name) {
            "* "
        } else {
            "  "
        };
        println!("{mark}{:<16} [{}]", p.name, p.tools.join(", "));
    }
    Ok(0)
}

pub fn status(paths: &Paths) -> Result<i32> {
    let store = Store::open(paths)?;
    for adapter in adapters::all() {
        let tool = adapter.name();
        match adapter.identity(paths)? {
            None => println!("{tool}: not logged in"),
            Some(id) => {
                let name = matched_profile(&store, tool, &id.account_id);
                let saved = match &name {
                    Some(n) => format!("profile '{n}'"),
                    None => "not saved - run `swapdex add <name>`".to_string(),
                };
                let exp = expiry_note(id.expires_at);
                println!("{tool}: {} ({saved}){exp}", identity_line(&id));
            }
        }
    }
    // A1: warn about the world-readable .claude.json (holds account PII).
    if let Ok(meta) = std::fs::metadata(paths.claude_config_json()) {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            println!(
                "note: {} is group/world-readable (holds your account email/org); `chmod 600` it",
                crate::util::redact_path(&paths.claude_config_json().display().to_string())
            );
        }
    }
    // Ecosystem: best-effort session count grouped by account (session_link).
    if let Some(line) = crate::session_link::status_line(paths) {
        println!("{line}");
    }
    Ok(0)
}

pub fn rm(paths: &Paths, name: &str) -> Result<i32> {
    let store = Store::open(paths)?;
    if !store.remove(name)? {
        eprintln!("swapdex: no profile named '{name}'");
        return Ok(5);
    }
    println!("removed profile '{name}' (any live login it matched keeps running, now unsaved)");
    Ok(0)
}

pub fn sessions(paths: &Paths) -> Result<i32> {
    match crate::session_link::sessions_by_account(paths) {
        None => {
            println!("session data unavailable (install sessionwiki for `sessions --by-account`)");
        }
        Some(counts) if counts.is_empty() => {
            println!("no sessions found");
        }
        Some(counts) => {
            for (account, n) in &counts {
                println!("{:<20} {n}", account);
            }
        }
    }
    Ok(0)
}

fn identity_line(id: &Account) -> String {
    let who = id.email.clone().unwrap_or_else(|| id.display.clone());
    match &id.tier {
        Some(t) => format!("{who} [{t}]"),
        None => who,
    }
}

fn expiry_note(expires_at: Option<i64>) -> String {
    // expiresAt is epoch millis. Just flag if already past; no live clock math
    // needed for a coarse warning.
    match expires_at {
        Some(ms) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if ms < now_ms {
                " - access token expired, may re-prompt".to_string()
            } else {
                String::new()
            }
        }
        None => String::new(),
    }
}

fn warn_if_expired(target: &crate::adapters::Snapshot, tool: &str) {
    if tool != "claude-code" {
        return;
    }
    if let Some(cred) = target.part("credentials") {
        if let Ok(v) = serde_json::from_slice::<Value>(cred.expose()) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if let Some(ms) = v["claudeAiOauth"]["expiresAt"].as_i64() {
                if ms < now_ms {
                    eprintln!("swapdex: note - this saved login's access token expired; the tool may re-prompt for login");
                }
            }
        }
    }
}
