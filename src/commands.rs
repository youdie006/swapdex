//! The subcommand handlers. Each returns an exit code; a hard error propagates
//! and `main` prints a redacted message + exits 1. Output is identity-based and
//! never prints a credential byte (the A11 egress guarantee) - the only reader
//! of a `Secret` is inside the adapters/store.

use crate::adapters::{self, Account, AuthTool};
use crate::paths::Paths;
use crate::store::Store;
use anyhow::Result;
use serde_json::Value;
use std::process::Command;

/// Is a CLI on PATH and runnable?
fn command_exists(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Which tool a `--tool` value targets. A clap `ValueEnum`, so an unknown or
/// miscased value (`--tool cluade`) is rejected with a did-you-mean instead of
/// silently falling through to "both" and switching a tool you meant to leave
/// alone. `None` (no `--tool`) means the default: act on whichever tools apply.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ToolSel {
    #[value(alias = "claude-code")]
    Claude,
    Codex,
    Both,
}

impl ToolSel {
    fn wants(self, tool: &str) -> bool {
        match self {
            ToolSel::Claude => tool == "claude-code",
            ToolSel::Codex => tool == "codex",
            ToolSel::Both => true,
        }
    }
}

/// The adapters a command targets. `None` and `Some(Both)` mean all; an explicit
/// single tool narrows it.
fn selected_adapters(sel: Option<ToolSel>) -> Vec<Box<dyn AuthTool>> {
    adapters::all()
        .into_iter()
        .filter(|a| sel.map(|s| s.wants(a.name())).unwrap_or(true))
        .collect()
}

/// Whether the user explicitly asked for one tool (so a missing one is an error,
/// not a silent skip). `--tool both` is treated as the lenient default.
fn is_explicit(sel: Option<ToolSel>) -> bool {
    matches!(sel, Some(ToolSel::Claude) | Some(ToolSel::Codex))
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

pub fn add(paths: &Paths, name: &str, sel: Option<ToolSel>, update: bool) -> Result<i32> {
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
    let mut skipped = Vec::new();
    for adapter in selected_adapters(sel) {
        let tool = adapter.name();
        if !adapter.present(paths) {
            if is_explicit(sel) {
                eprintln!("swapdex: not logged in to {tool}");
                return Ok(3);
            }
            continue;
        }
        if store.load(name, tool)?.is_some() && !update {
            // Explicit --tool on an already-saved tool is an error; in the
            // default case, just skip it and still attach the missing tool(s).
            if is_explicit(sel) {
                eprintln!(
                    "swapdex: profile '{name}' already has a {tool} login; pass --update to replace"
                );
                return Ok(6);
            }
            skipped.push(tool);
            continue;
        }
        let snap = adapter.capture(paths)?;
        store.save(name, &snap)?;
        saved.push(tool);
    }
    if saved.is_empty() {
        if !skipped.is_empty() {
            eprintln!(
                "swapdex: profile '{name}' already has {}; pass --update to replace",
                skipped.join(", ")
            );
            return Ok(6);
        }
        eprintln!("swapdex: not logged in to any selected tool");
        return Ok(3);
    }
    let note = if skipped.is_empty() {
        String::new()
    } else {
        format!(
            " ({} already saved; --update to replace)",
            skipped.join(", ")
        )
    };
    println!("saved profile '{name}' ({}){note}", saved.join(", "));
    Ok(0)
}

pub fn use_account(paths: &Paths, name: &str, sel: Option<ToolSel>, dry_run: bool) -> Result<i32> {
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
    let mut matched = 0; // profile had a snapshot for this tool
    let mut changed = 0; // an actual switch was written
    // Snapshot running processes once (best-effort) so we can warn if a switch
    // pulls the login out from under a live session. Skipped on a dry-run.
    let running = if dry_run {
        Vec::new()
    } else {
        crate::proc::running_process_names()
    };
    for adapter in selected_adapters(sel) {
        let tool = adapter.name();
        let target = match store.load(name, tool)? {
            Some(s) => s,
            None => {
                if is_explicit(sel) {
                    eprintln!("swapdex: profile '{name}' has no {tool} login");
                    return Ok(5);
                }
                continue;
            }
        };
        matched += 1;
        // Already-active is a no-op success. Ignore EMPTY ids: two accounts with
        // no account_id must never compare equal, or the switch would be skipped
        // and the WRONG account silently kept active.
        let live_id = adapter
            .identity(paths)?
            .map(|i| i.account_id)
            .filter(|s| !s.is_empty());
        let target_id = profile_account_id(&store, name, tool).filter(|s| !s.is_empty());
        if live_id.is_some() && live_id == target_id {
            println!("{tool}: '{name}' is already active");
            continue;
        }
        warn_if_expired(&target, tool);
        if dry_run {
            println!("would switch {tool} -> {name}");
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
        if let Some(id) = adapter.identity(paths)? {
            println!("switched {tool} -> {}", identity_line(&id));
        }
        if crate::proc::tool_running(tool, &running) {
            eprintln!(
                "swapdex: note - a {tool} session looks like it's running. Restart it \
                 to use '{name}'; a live session can overwrite the switched login on \
                 its next token refresh."
            );
        }
        changed += 1;
    }
    if matched == 0 {
        eprintln!("swapdex: no profile named '{name}'");
        return Ok(5);
    }
    // Only when a login was actually written - not for a no-op or a dry-run.
    if changed > 0 {
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

/// Summarize a profile across ALL its tools (not just the first): a marker if
/// ANY tool is stale/expired, and the first non-empty email/tier. `p.tools` is
/// alphabetical, so inspecting only the first would always be "claude-code" and
/// hide Codex entirely.
fn profile_summary(
    store: &Store,
    name: &str,
    tools: &[String],
) -> (Option<String>, Option<String>, Option<&'static str>) {
    let mut email = None;
    let mut tier = None;
    let mut marker = None;
    for t in tools {
        if let Some((e, ti, m)) = profile_detail(store, name, t) {
            email = email.or(e);
            tier = tier.or(ti);
            marker = marker.or(m);
        }
    }
    (email, tier, marker)
}

/// Which profile is the LIVE account for each tool (from live identity, A2).
/// A mixed state (claude on profile X, codex on profile Y) is representable.
pub(crate) fn active_by_tool(store: &Store, paths: &Paths) -> Vec<(&'static str, String)> {
    adapters::all()
        .iter()
        .filter_map(|a| {
            a.identity(paths)
                .ok()
                .flatten()
                .and_then(|id| matched_profile_name(store, a.name(), &id.account_id))
                .map(|name| (a.name(), name))
        })
        .collect()
}

/// The account column: email if known, else just the tier (never a stray
/// leading-space " [tier]").
fn identity_column(email: Option<String>, tier: Option<String>) -> String {
    match (email.filter(|e| !e.is_empty()), tier) {
        (Some(e), Some(t)) => format!("{e} [{t}]"),
        (Some(e), None) => e,
        (None, Some(t)) => format!("[{t}]"),
        (None, None) => String::new(),
    }
}

pub fn ls(paths: &Paths, json: bool) -> Result<i32> {
    let store = Store::open(paths)?;
    let active = active_by_tool(&store, paths);
    let active_tools_for = |name: &str| -> Vec<&'static str> {
        active
            .iter()
            .filter(|(_, n)| n == name)
            .map(|(t, _)| *t)
            .collect()
    };

    let profiles = store.list();
    if json {
        let rows: Vec<Value> = profiles
            .iter()
            .map(|p| {
                let (email, tier, marker) = profile_summary(&store, &p.name, &p.tools);
                serde_json::json!({
                    "name": p.name,
                    "tools": p.tools,
                    "active_tools": active_tools_for(&p.name),
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
        println!("No accounts saved yet.");
        println!("  guided setup:  swapdex setup");
        println!("  or add one:    swapdex login <name>");
        return Ok(0);
    }
    // Two-pass so columns fit the actual content (with a sane cap).
    struct Row {
        name: String,
        ident: String,
        tools: String,
        warn: Option<&'static str>,
        active: bool,
    }
    let rows: Vec<Row> = profiles
        .iter()
        .map(|p| {
            let (email, tier, marker) = profile_summary(&store, &p.name, &p.tools);
            let at = active_tools_for(&p.name);
            let tools = p
                .tools
                .iter()
                .map(|t| {
                    if at.contains(&t.as_str()) {
                        format!("{t}*")
                    } else {
                        t.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            Row {
                name: p.name.clone(),
                ident: identity_column(email, tier),
                tools,
                warn: marker,
                active: !at.is_empty(),
            }
        })
        .collect();
    let name_w = rows
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 24);
    let ident_w = rows
        .iter()
        .map(|r| r.ident.len())
        .max()
        .unwrap_or(0)
        .clamp(0, 40);
    let mut saw_marker = false;
    for r in &rows {
        let mark = if r.active { "* " } else { "  " };
        let warn = r.warn.map(|m| format!("  ({m})")).unwrap_or_default();
        saw_marker |= r.warn.is_some();
        println!(
            "{mark}{:<name_w$} {:<ident_w$} [{}]{warn}",
            r.name, r.ident, r.tools
        );
    }
    if saw_marker {
        println!(
            "  (expired/stale: re-run `swapdex add --update <name>` while logged in to refresh)"
        );
    }
    if active
        .iter()
        .map(|(_, n)| n)
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1
    {
        println!("  (* marks the active account per tool)");
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

/// Onboarding in one step: run a tool's login flow, then save the result as a
/// named profile. Codex has a driveable CLI login; Claude Code signs in inside
/// the app, so for it swapdex guides the two-step manual path.
pub fn login(paths: &Paths, name: &str, sel: Option<ToolSel>) -> Result<i32> {
    crate::atomic::ensure_not_root()?;
    if let Some(c) = reject_bad_name(name) {
        return Ok(c);
    }
    let tool = match sel {
        Some(ToolSel::Claude) => "claude-code",
        Some(ToolSel::Codex) => "codex",
        _ if command_exists("codex") => "codex",
        _ => "claude-code",
    };

    if tool == "claude-code" {
        if !command_exists("claude") {
            println!("Claude Code isn't on your PATH. Install it, then:");
            println!("  1) run `claude` and complete the login");
            println!("  2) then:  swapdex add {name} --tool claude");
            return Ok(0);
        }
        let claude = adapters::by_name("claude-code");
        let already = claude
            .as_ref()
            .and_then(|c| c.identity(paths).ok().flatten())
            .is_some();
        if already {
            // Already logged in. Adding a DIFFERENT account needs /logout + /login
            // inside a session - spawning `claude` won't re-prompt, so we guide.
            println!("You're already logged into Claude Code.");
            println!("  save the current account:  swapdex add {name} --tool claude");
            println!("  or switch to another account first: run `claude`, use /logout then");
            println!(
                "  /login with the other account, exit, then `swapdex add {name} --tool claude`."
            );
            return Ok(0);
        }
        // Not logged in: drive it. Claude Code has no login subcommand, so run
        // `claude` itself - its first-run flow does the browser login - then
        // auto-capture the credentials it writes.
        println!(
            "Opening Claude Code to sign in. Complete the login, then exit it (Ctrl-D or /exit)."
        );
        Command::new("claude")
            .status()
            .map_err(|e| anyhow::anyhow!("could not run claude: {e}"))?;
        let logged_in = adapters::by_name("claude-code")
            .and_then(|c| c.identity(paths).ok().flatten())
            .is_some();
        if !logged_in {
            eprintln!("swapdex: no Claude login was completed - nothing saved.");
            return Ok(8);
        }
        println!();
        return add(paths, name, Some(ToolSel::Claude), true);
    }

    if !command_exists("codex") {
        eprintln!("swapdex: the `codex` CLI is not on your PATH - install it, then retry.");
        return Ok(3);
    }
    // `codex login` DELETES ~/.codex/auth.json before its browser step, so an
    // interrupted login would otherwise lose the current login. Snapshot it into
    // the store's backups first (recoverable regardless of how login goes).
    if let Some(codex) = adapters::by_name("codex") {
        if codex.present(paths) {
            if let Ok(snap) = codex.capture(paths) {
                let _ = Store::open(paths).and_then(|s| s.backup(&snap));
            }
        }
    }
    println!("Opening `codex login` - sign in with the account to save as '{name}'.");
    println!("(If it says you're already logged in, run `codex logout` first, then retry.)");
    // Inherit the terminal so the browser/device prompt shows and no child stdio
    // is captured into swapdex output.
    let status = Command::new("codex")
        .arg("login")
        .status()
        .map_err(|e| anyhow::anyhow!("could not run codex login: {e}"))?;
    if !status.success() {
        eprintln!("swapdex: codex login did not complete - nothing saved.");
        return Ok(8);
    }
    println!();
    // update=true so re-running `login <name>` refreshes an existing profile.
    add(paths, name, Some(ToolSel::Codex), true)
}

/// Ask a question and read a trimmed line; empty input yields `default`.
fn prompt(question: &str, default: &str) -> String {
    use std::io::Write;
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return default.to_string();
    }
    let t = line.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

/// A default profile name from an email/display (its local part, sanitized).
fn suggest_name(who: &str) -> String {
    let base = who.split('@').next().unwrap_or(who);
    let clean: String = base
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if crate::store::valid_profile_name(&clean) {
        clean
    } else {
        "account".to_string()
    }
}

/// A friendly tool label for prompts.
fn pretty_tool(tool: &str) -> &str {
    match tool {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        other => other,
    }
}

/// A yes/no prompt; empty input takes `default_yes`.
fn yes_no(question: &str, default_yes: bool) -> bool {
    let a = prompt(question, if default_yes { "y" } else { "n" });
    matches!(a.to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Ask for a profile name, re-prompting until it is valid (or the user skips).
/// An existing name asks whether to replace it. Returns None on skip.
fn ask_name(store: &Store, question: &str, default: &str) -> Option<String> {
    loop {
        let ans = prompt(question, default);
        if ans.eq_ignore_ascii_case("skip") || ans.is_empty() {
            return None;
        }
        if !crate::store::valid_profile_name(&ans) {
            println!("  '{ans}' can't be a name (no '/', '..', leading '.', or control chars). Try again.");
            continue;
        }
        if store.list().iter().any(|p| p.name == ans)
            && !yes_no(
                &format!("  '{ans}' already exists - replace it? [y/N]: "),
                false,
            )
        {
            continue;
        }
        return Some(ans);
    }
}

/// Guided first-run onboarding: save the accounts you're logged into, offer to
/// add more, and show how to switch. Interactive (needs a TTY).
pub fn setup(paths: &Paths) -> Result<i32> {
    use std::io::IsTerminal;
    crate::atomic::ensure_not_root()?;
    // SWAPDEX_ASSUME_TTY lets the test suite drive the prompts over a pipe.
    if !std::io::stdin().is_terminal() && std::env::var_os("SWAPDEX_ASSUME_TTY").is_none() {
        eprintln!(
            "swapdex setup is interactive - run it in a terminal, or use `swapdex login <name>`."
        );
        return Ok(1);
    }
    let store = Store::open(paths)?;
    println!("swapdex keeps several Claude Code / Codex logins and switches between them.");
    println!(
        "Let's save the accounts you use. Press Enter to accept a [default], Ctrl-C to quit.\n"
    );

    // 1) Save the accounts you're currently logged into.
    for adapter in adapters::all() {
        let tool = adapter.name();
        let id = match adapter.identity(paths)? {
            Some(id) => id,
            None => {
                println!("{}: not logged in - skipping.\n", pretty_tool(tool));
                continue;
            }
        };
        if let Some(existing) = matched_profile_name(&store, tool, &id.account_id) {
            println!("{}: already saved as '{existing}'.\n", pretty_tool(tool));
            continue;
        }
        let who = id.email.clone().unwrap_or_else(|| id.display.clone());
        let default = suggest_name(&who);
        println!("{}: you're logged in as {who}.", pretty_tool(tool));
        match ask_name(
            &store,
            &format!("  save it as [{default}] (Enter to accept, 'skip' to skip): "),
            &default,
        ) {
            Some(name) => {
                let snap = adapter.capture(paths)?;
                store.save(&name, &snap)?;
                println!("  saved as '{name}'.\n");
            }
            None => println!("  skipped.\n"),
        }
    }

    // 2) Offer to add more Codex accounts (the one with a driveable login).
    if command_exists("codex") {
        println!("You can keep several Codex accounts (e.g. work and personal).");
        while yes_no("  add another Codex account now? [y/N]: ", false) {
            let name = match ask_name(&store, "  name for it (e.g. personal): ", "") {
                Some(n) => n,
                None => {
                    println!("  skipped.\n");
                    continue;
                }
            };
            println!(
                "  This logs out of the current Codex account and opens a fresh browser login."
            );
            println!("  (Your current login is backed up first, so nothing is lost.)");
            if !yes_no("  continue? [y/N]: ", false) {
                println!("  cancelled.\n");
                continue;
            }
            // Back up the current login before `codex login` deletes it.
            if let Some(codex) = adapters::by_name("codex") {
                if codex.present(paths) {
                    if let Ok(s) = codex.capture(paths) {
                        let _ = store.backup(&s);
                    }
                }
            }
            let _ = Command::new("codex").arg("logout").status();
            println!("  opening codex login - complete the sign-in in your browser...");
            let ok = Command::new("codex")
                .arg("login")
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                println!("  login didn't finish; nothing saved.\n");
                continue;
            }
            if let Some(codex) = adapters::by_name("codex") {
                if codex.present(paths) {
                    let snap = codex.capture(paths)?;
                    store.save(&name, &snap)?;
                    println!("  saved '{name}'.\n");
                }
            }
        }
    }

    // 3) Summary.
    let names: Vec<String> = store.list().into_iter().map(|p| p.name).collect();
    println!();
    if names.is_empty() {
        println!(
            "No accounts saved yet. Log into Claude Code or Codex, then run `swapdex setup` again."
        );
    } else {
        println!("You're set - saved: {}.", names.join(", "));
        println!("  switch:   swapdex use <name>");
        println!("  see all:  swapdex ls");
        if names.len() > 1 {
            println!("Switching takes effect on your next message - no restart needed.");
        }
    }
    Ok(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn usage(paths: &Paths, json: bool) -> Result<i32> {
    let rows = crate::usage::tool_usage(paths);
    if json {
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "tool": r.tool,
                    "last_5h": {"sessions": r.w5h.sessions, "tokens": r.w5h.tokens},
                    "last_7d": {"sessions": r.w7d.sessions, "tokens": r.w7d.tokens},
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&out)?);
        return Ok(0);
    }
    if rows.iter().all(|r| r.w7d.sessions == 0) {
        println!("No recent session activity found (reads ~/.claude and ~/.codex, locally).");
        return Ok(0);
    }
    println!("Local usage - this machine, approximate (not the billed quota):");
    for r in &rows {
        if r.w7d.sessions == 0 {
            continue;
        }
        println!(
            "  {:<12} 5h: {:>7} tok / {} sess    7d: {:>8} tok / {} sess",
            r.tool,
            crate::usage::human(r.w5h.tokens),
            r.w5h.sessions,
            crate::usage::human(r.w7d.tokens),
            r.w7d.sessions,
        );
    }
    println!("(tokens are summed from local session transcripts; not tagged by account)");
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
