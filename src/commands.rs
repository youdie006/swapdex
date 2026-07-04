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
pub(crate) fn matched_profile_name(store: &Store, tool: &str, live_id: &str) -> Option<String> {
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

/// Reject a profile name that could escape the store (path traversal). Returns
/// the exit code to use if invalid.
fn reject_bad_name(name: &str) -> Option<i32> {
    if crate::store::valid_profile_name(name) {
        None
    } else {
        eprintln!("swapdex: invalid profile name '{name}' (no '/', '\\', '..', leading '.', or control chars)");
        Some(2)
    }
}

pub fn add(paths: &Paths, name: &str, sel: &ToolSel, update: bool) -> Result<i32> {
    crate::atomic::ensure_not_root()?;
    if let Some(c) = reject_bad_name(name) {
        return Ok(c);
    }
    let store = Store::open(paths)?;
    // Take the switch lock so `add --update` can't race a `use` into a torn
    // (mismatched credentials + identity) two-file Claude snapshot.
    let _lock = match store.lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("swapdex: another swapdex is mid-switch; try again");
            return Ok(4);
        }
    };
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
    if let Some(c) = reject_bad_name(name) {
        return Ok(c);
    }
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
        // Already-active is a no-op success: never re-write the live credential
        // (which would re-open the refresh-token race), churn backups, or append
        // a duplicate timeline event (which would skew session attribution).
        let live_id = adapter.identity(paths)?.map(|i| i.account_id);
        let target_id = profile_account_id(&store, name, tool);
        if live_id.is_some() && live_id == target_id {
            println!("{tool}: '{name}' is already active");
            switched += 1;
            continue;
        }
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

/// A saved snapshot ages out even before its access token expires, because the
/// refresh token can rotate; flag one that has not been refreshed in a while.
const STALE_DAYS: i64 = 30;

/// Identity extracted from a STORED snapshot (no live read, no secrets):
/// (email, tier, marker). marker is "expired" (Claude access token past expiry)
/// or "stale" (Codex login not refreshed in >STALE_DAYS - its refresh token may
/// have rotated, so re-run `add --update`), else None.
fn profile_detail(
    store: &Store,
    name: &str,
    tool: &str,
) -> Option<(Option<String>, Option<String>, Option<&'static str>)> {
    let snap = store.load(name, tool).ok()??;
    match tool {
        "claude-code" => {
            let creds: Value = serde_json::from_slice(snap.part("credentials")?.expose()).ok()?;
            let oauth: Value = serde_json::from_slice(snap.part("oauth_account")?.expose()).ok()?;
            let marker = match creds["claudeAiOauth"]["expiresAt"].as_i64() {
                Some(ms) if ms < now_ms() => Some("expired"),
                _ => None,
            };
            Some((
                oauth["emailAddress"].as_str().map(String::from),
                creds["claudeAiOauth"]["subscriptionType"]
                    .as_str()
                    .map(String::from),
                marker,
            ))
        }
        "codex" => {
            let auth: Value = serde_json::from_slice(snap.part("auth")?.expose()).ok()?;
            let email = crate::adapters::codex::decode_email_from_id_token(
                auth["tokens"]["id_token"].as_str(),
            );
            let marker = auth["last_refresh"]
                .as_str()
                .and_then(crate::session_link::rfc3339_to_secs)
                .filter(|&secs| now_ms() / 1000 - secs > STALE_DAYS * 86400)
                .map(|_| "stale");
            Some((email, auth["auth_mode"].as_str().map(String::from), marker))
        }
        _ => None,
    }
}

pub fn ls(paths: &Paths, json: bool) -> Result<i32> {
    let store = Store::open(paths)?;
    // Active markers come from LIVE identity, matched back to a profile (A2).
    let active_names: Vec<String> = adapters::all()
        .iter()
        .filter_map(|a| {
            a.identity(paths)
                .ok()
                .flatten()
                .and_then(|id| matched_profile_name(&store, a.name(), &id.account_id))
        })
        .collect();

    let profiles = store.list();
    if json {
        let rows: Vec<Value> = profiles
            .iter()
            .map(|p| {
                let (email, tier, marker) = p
                    .tools
                    .first()
                    .and_then(|t| profile_detail(&store, &p.name, t))
                    .unwrap_or((None, None, None));
                serde_json::json!({
                    "name": p.name,
                    "tools": p.tools,
                    "active": active_names.contains(&p.name),
                    "email": email,
                    "tier": tier,
                    "warning": marker,
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
    let mut saw_marker = false;
    for p in &profiles {
        let mark = if active_names.contains(&p.name) {
            "* "
        } else {
            "  "
        };
        let (email, tier, marker) = p
            .tools
            .first()
            .and_then(|t| profile_detail(&store, &p.name, t))
            .unwrap_or((None, None, None));
        let who = email.unwrap_or_default();
        let tier = tier.map(|t| format!(" [{t}]")).unwrap_or_default();
        let warn = marker.map(|m| format!("  ({m})")).unwrap_or_default();
        saw_marker |= marker.is_some();
        println!(
            "{mark}{:<16} {:<26} [{}]{warn}",
            p.name,
            format!("{who}{tier}"),
            p.tools.join(", ")
        );
    }
    if saw_marker {
        println!(
            "  (expired/stale: re-run `swapdex add --update <name>` while logged in to refresh)"
        );
    }
    Ok(0)
}

pub fn status(paths: &Paths, json: bool) -> Result<i32> {
    let store = Store::open(paths)?;
    if json {
        let rows: Vec<Value> = adapters::all()
            .iter()
            .map(|adapter| {
                let tool = adapter.name();
                match adapter.identity(paths).ok().flatten() {
                    None => serde_json::json!({"tool": tool, "logged_in": false}),
                    Some(id) => serde_json::json!({
                        "tool": tool,
                        "logged_in": true,
                        "email": id.email,
                        "tier": id.tier,
                        "profile": matched_profile_name(&store, tool, &id.account_id),
                        "expired": id.expires_at.map(|ms| ms < now_ms()),
                    }),
                }
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(0);
    }
    for adapter in adapters::all() {
        let tool = adapter.name();
        match adapter.identity(paths)? {
            None => println!("{tool}: not logged in"),
            Some(id) => {
                let name = matched_profile_name(&store, tool, &id.account_id);
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

pub fn rm(paths: &Paths, name: &str, yes: bool) -> Result<i32> {
    if let Some(c) = reject_bad_name(name) {
        return Ok(c);
    }
    let store = Store::open(paths)?;
    if !yes {
        eprintln!("swapdex: `rm {name}` deletes the saved profile. Re-run with --yes to confirm.");
        return Ok(7);
    }
    let _lock = match store.lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("swapdex: another swapdex is mid-switch; try again");
            return Ok(4);
        }
    };
    if !store.remove(name)? {
        eprintln!("swapdex: no profile named '{name}'");
        return Ok(5);
    }
    println!("removed profile '{name}' (any live login it matched keeps running, now unsaved)");
    Ok(0)
}

pub fn rename(paths: &Paths, old: &str, new: &str) -> Result<i32> {
    if let Some(c) = reject_bad_name(old) {
        return Ok(c);
    }
    if let Some(c) = reject_bad_name(new) {
        return Ok(c);
    }
    let store = Store::open(paths)?;
    if store.rename(old, new)? {
        println!("renamed profile '{old}' -> '{new}'");
        Ok(0)
    } else {
        eprintln!("swapdex: no profile named '{old}'");
        Ok(5)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
        Some(ms) if ms < now_ms() => " - access token expired, may re-prompt".to_string(),
        _ => String::new(),
    }
}

fn warn_if_expired(target: &crate::adapters::Snapshot, tool: &str) {
    if tool != "claude-code" {
        return;
    }
    if let Some(cred) = target.part("credentials") {
        if let Ok(v) = serde_json::from_slice::<Value>(cred.expose()) {
            if let Some(ms) = v["claudeAiOauth"]["expiresAt"].as_i64() {
                if ms < now_ms() {
                    eprintln!("swapdex: note - this saved login's access token expired; the tool may re-prompt for login");
                }
            }
        }
    }
}
