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

/// On macOS, Claude Code keeps its OAuth login in the Keychain rather than in
/// `~/.claude/.credentials.json`, so swapdex sees "not logged in" even when a
/// login exists. When the config file proves a login is present, explain that
/// instead of gaslighting the user. (`cfg!` keeps this type-checked on Linux.)
fn macos_keychain_note(paths: &Paths, tool: &str) -> Option<&'static str> {
    if !cfg!(target_os = "macos") || tool != "claude-code" {
        return None;
    }
    if paths.claude_credentials().exists() {
        return None;
    }
    let logged_in_by_config = std::fs::read(paths.claude_config_json())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| v["oauthAccount"].is_object())
        .unwrap_or(false);
    if logged_in_by_config {
        Some(
            "Claude Code on macOS keeps its login in the Keychain, which swapdex \
             cannot snapshot yet - Codex switching works; Claude-on-macOS is on \
             the roadmap",
        )
    } else {
        None
    }
}

/// The account_id inside a snapshot's blobs (works for stored profiles and
/// backups alike).
fn snapshot_account_id(snap: &crate::adapters::Snapshot, tool: &str) -> Option<String> {
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

/// The account_id a stored profile's snapshot resolves to, for matching a live
/// identity back to a profile name (A2). Reads the snapshot, not `active.json`.
fn profile_account_id(store: &Store, name: &str, tool: &str) -> Option<String> {
    let snap = store.load(name, tool).ok()??;
    snapshot_account_id(&snap, tool)
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

/// Additionally reject "-" where a profile is CREATED (`use -` toggles, so a
/// new profile must never take that name; a legacy one stays manageable).
fn reject_reserved_name(name: &str) -> Option<i32> {
    if name == "-" {
        eprintln!("swapdex: '-' is reserved (`swapdex use -` toggles to the previous profile)");
        Some(2)
    } else {
        None
    }
}

pub fn add(paths: &Paths, name: Option<&str>, sel: Option<ToolSel>, update: bool) -> Result<i32> {
    crate::atomic::ensure_not_root()?;
    let store = Store::open(paths)?;
    // No name: on a terminal, suggest one from the live account (setup's flow);
    // non-interactively, error with the fix instead of a bare usage error.
    let asked;
    let name: &str = match name {
        Some(n) => n,
        None => {
            use std::io::IsTerminal;
            let tty =
                std::io::stdin().is_terminal() || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
            if !tty {
                eprintln!(
                    "swapdex: a profile name is required: swapdex add <name> \
                     (or run `swapdex setup` for the guided flow)"
                );
                return Ok(2);
            }
            let who = adapters::all()
                .iter()
                .find_map(|a| a.identity(paths).ok().flatten())
                .map(|id| id.email.unwrap_or(id.display))
                .unwrap_or_else(|| "account".into());
            let suggestion = suggest_name(&who);
            match ask_name(
                &store,
                &format!("name for this account [{suggestion}]: "),
                &suggestion,
            ) {
                Some(n) => {
                    asked = n;
                    &asked
                }
                None => {
                    println!("nothing saved.");
                    return Ok(0);
                }
            }
        }
    };
    if let Some(c) = reject_bad_name(name).or_else(|| reject_reserved_name(name)) {
        return Ok(c);
    }
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
                if let Some(note) = macos_keychain_note(paths, tool) {
                    eprintln!("swapdex: note - {note}");
                }
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
    if name.contains(char::is_whitespace) {
        println!(
            "note: the name has spaces - quote it in later commands (`swapdex use \"{name}\"`)"
        );
    }
    Ok(0)
}

pub fn use_account(paths: &Paths, name: &str, sel: Option<ToolSel>, dry_run: bool) -> Result<i32> {
    crate::atomic::ensure_not_root()?;
    let store = Store::open(paths)?;
    // Resolve the NAME first: `-` toggles to the previous/other profile and a
    // unique prefix expands, so the daily switch is two keystrokes.
    let name = match resolve_use_name(&store, paths, name, sel)? {
        Some(n) => n,
        None => return Ok(5),
    };
    let name = name.as_str();
    if let Some(c) = reject_bad_name(name) {
        return Ok(c);
    }
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
    // Skip the scan on a dry-run, and under SWAPDEX_ROOT: an isolated root's
    // credentials are not the ones any running session uses, so the warning
    // would be a false positive there.
    let running = if dry_run || std::env::var_os("SWAPDEX_ROOT").is_some() {
        Vec::new()
    } else {
        crate::proc::running_process_names()
    };
    // One shared timestamp for every tool this invocation switches, so a later
    // bare `restore` can identify exactly this switch's tool set.
    let switch_ts = now_secs();
    for adapter in selected_adapters(sel) {
        let tool = adapter.name();
        // A Keychain-mode Claude install (macOS) cannot be switched yet. In
        // the default both-tools case SKIP it with a note so Codex still
        // switches; the adapter's own refusal stays for an explicit --tool.
        if !is_explicit(sel) && macos_keychain_note(paths, tool).is_some() {
            println!(
                "{tool}: skipped - the login lives in the macOS Keychain \
                 (github.com/youdie006/swapdex/issues/1); other tools continue"
            );
            continue;
        }
        let target = match store.load(name, tool)? {
            Some(s) => s,
            None => {
                if is_explicit(sel) {
                    eprintln!("swapdex: profile '{name}' has no {tool} login");
                    return Ok(5);
                }
                // Not an error in the default case, but if the user IS logged
                // into this tool, say so - a silent partial switch reads as a
                // full one and leaves the old account active unnoticed.
                if adapter.present(paths) {
                    println!("{tool}: profile '{name}' has no {tool} login - left unchanged");
                }
                continue;
            }
        };
        matched += 1;
        // Already-active is a no-op success. Ignore EMPTY ids: two accounts with
        // no account_id must never compare equal, or the switch would be skipped
        // and the WRONG account silently kept active. An UNREADABLE live file is
        // treated as unknown (not an abort): `use <good-profile>` is exactly the
        // command that can replace a corrupt login.
        let live = adapter.identity(paths).ok().flatten();
        let live_id = live
            .as_ref()
            .map(|i| i.account_id.clone())
            .filter(|s| !s.is_empty());
        let target_id = profile_account_id(&store, name, tool).filter(|s| !s.is_empty());
        if live_id.is_some() && live_id == target_id {
            println!("{tool}: '{name}' is already active");
            continue;
        }
        warn_if_expired(&target, tool);
        if dry_run {
            match profile_detail(&store, name, tool).and_then(|(email, _, _)| email) {
                Some(email) => println!("would switch {tool} -> {name} ({email})"),
                None => println!("would switch {tool} -> {name}"),
            }
            continue;
        }
        // Safe order (A6): back up the CURRENT live login first (atomic + fsync
        // inside write_secret); if the backup fails, `?` aborts BEFORE we touch
        // the live login. An unreadable live file only skips its own backup -
        // there is nothing usable to save.
        if adapter.present(paths) {
            match adapter.capture(paths) {
                Ok(live_snap) => {
                    store.backup(&live_snap)?;
                    // Only the last 2 backups remember an account that is not
                    // saved as a profile - warn while it is still recoverable.
                    if let Some(id) = &live_id {
                        if matched_profile_name(&store, tool, id).is_none() {
                            let who = live
                                .as_ref()
                                .map(identity_line)
                                .unwrap_or_else(|| "current".into());
                            eprintln!(
                                "swapdex: note - the outgoing {tool} login ({who}) is not \
                                 saved as a profile; only the last 2 backups keep it. \
                                 `swapdex restore` undoes this switch; `swapdex add <name>` \
                                 would keep it for good."
                            );
                        }
                    }
                }
                Err(e) => eprintln!(
                    "swapdex: note - the current {tool} login could not be read ({e:#}); \
                     switching without a backup of it"
                ),
            }
        }
        adapter.apply(paths, &target).map_err(|e| {
            e.context(format!(
                "profile '{name}' has a bad {tool} snapshot - log in to that account \
                 and re-save it with `swapdex add {name} --tool {tool} --update`"
            ))
        })?;
        store.append_timeline_at(tool, name, "use", switch_ts)?;
        if let Some(id) = adapter.identity(paths).ok().flatten() {
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

/// `restore` - put back the login that was live before the last switch. `use`
/// backs up the outgoing login before every switch; this is the command that
/// brings a backup back, so a bad switch is a one-command recovery even when
/// the outgoing account was never saved as a profile. It backs up the current
/// login first, so running `restore` twice toggles between the two.
pub fn restore(paths: &Paths, sel: Option<ToolSel>, dry_run: bool) -> Result<i32> {
    crate::atomic::ensure_not_root()?;
    let store = Store::open(paths)?;
    let _lock = match store.lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("swapdex: another swapdex is mid-switch; try again");
            return Ok(4);
        }
    };
    // Skip the scan on a dry-run, and under SWAPDEX_ROOT: an isolated root's
    // credentials are not the ones any running session uses, so the warning
    // would be a false positive there.
    let running = if dry_run || std::env::var_os("SWAPDEX_ROOT").is_some() {
        Vec::new()
    } else {
        crate::proc::running_process_names()
    };
    // Bare `restore` means "undo the LAST SWITCH" - scope it to the tool(s)
    // that switch touched, or a codex-only undo would also rewind claude-code
    // to some older, unrelated backup.
    let last_switch = last_switch_tools(paths);
    let restore_ts = now_secs();
    let mut found = 0; // a backup existed for this tool
    let mut changed = 0; // an actual restore was written
    for adapter in selected_adapters(sel) {
        let tool = adapter.name();
        if !is_explicit(sel) {
            if let Some(tools) = &last_switch {
                if !tools.iter().any(|t| t == tool) {
                    continue;
                }
            }
            // Keychain-mode Claude (macOS): skip with a note, keep restoring
            // the other tool (mirror of the `use` skip).
            if macos_keychain_note(paths, tool).is_some() {
                println!(
                    "{tool}: skipped - the login lives in the macOS Keychain \
                     (github.com/youdie006/swapdex/issues/1); other tools continue"
                );
                continue;
            }
        }
        let Some((stamp, target)) = store.load_backup(tool)? else {
            if is_explicit(sel) {
                eprintln!("swapdex: no backup for {tool} (a backup is taken on every `use`)");
                return Ok(5);
            }
            continue;
        };
        found += 1;
        // Restoring the already-live account is a no-op success. An unreadable
        // live file is treated as unknown, not an abort - restore is the
        // disaster-recovery command.
        let live_id = adapter
            .identity(paths)
            .ok()
            .flatten()
            .map(|i| i.account_id)
            .filter(|s| !s.is_empty());
        let backup_id = snapshot_account_id(&target, tool).filter(|s| !s.is_empty());
        if live_id.is_some() && live_id == backup_id {
            println!("{tool}: the newest backup is already the active login");
            continue;
        }
        let age = age_line(stamp);
        if dry_run {
            println!("would restore {tool} from the backup taken {age}");
            continue;
        }
        // Same safe order as `use`: back up the CURRENT login first, so restore
        // is itself reversible (run it again to toggle back).
        if adapter.present(paths) {
            match adapter.capture(paths) {
                Ok(live) => store.backup(&live)?,
                Err(e) => eprintln!(
                    "swapdex: note - the current {tool} login could not be read ({e:#}); \
                     restoring without a backup of it"
                ),
            }
        }
        adapter.apply(paths, &target)?;
        // Attribute the timeline event to the restored account's profile name
        // when one matches, or `sessions` would blame "(backup)" forever after.
        let restored = adapter.identity(paths).ok().flatten();
        let event_name = restored
            .as_ref()
            .and_then(|id| matched_profile_name(&store, tool, &id.account_id))
            .unwrap_or_else(|| "(backup)".into());
        store.append_timeline_at(tool, &event_name, "restore", restore_ts)?;
        match restored {
            Some(id) => println!("restored {tool} -> {} (backup {age})", identity_line(&id)),
            None => println!("restored {tool} from the backup taken {age}"),
        }
        if crate::proc::tool_running(tool, &running) {
            eprintln!(
                "swapdex: note - a {tool} session looks like it's running. Restart it \
                 to pick up the restored login."
            );
        }
        changed += 1;
    }
    if found == 0 {
        eprintln!("swapdex: no backup to restore (a backup is taken on every `use`)");
        return Ok(5);
    }
    if changed > 0 {
        println!("(takes effect on your next message)");
    }
    Ok(0)
}

/// Resolve `use`'s NAME argument. `-` means "the profile I was on before":
/// with exactly two profiles it is simply the other one; otherwise the most
/// recent timeline switch to a profile that is not currently active. A unique
/// prefix expands (`use w` -> work); an ambiguous one refuses and lists the
/// candidates rather than guessing (switching is a write). `Ok(None)` means
/// "already reported, exit 5".
fn resolve_use_name(
    store: &Store,
    paths: &Paths,
    raw: &str,
    sel: Option<ToolSel>,
) -> Result<Option<String>> {
    // An empty name (an unset shell variable) must fall through to the
    // invalid-name rejection: every string starts with "", so prefix matching
    // would otherwise "uniquely" match a single-profile store and switch.
    if raw.is_empty() {
        return Ok(Some(raw.to_string()));
    }
    let profiles: Vec<String> = store.list().into_iter().map(|p| p.name).collect();
    if raw == "-" {
        // Scope "previous" to the selected tool(s): `use - --tool codex` asks
        // about codex history, not claude's.
        let mut act: Vec<String> = active_by_tool(store, paths)
            .into_iter()
            .filter(|(t, _)| sel.map(|s| s.wants(t)).unwrap_or(true))
            .map(|(_, n)| n)
            .collect();
        act.sort();
        act.dedup();
        // The overwhelmingly common case: two profiles, one active.
        if profiles.len() == 2 && act.len() == 1 {
            if let Some(other) = profiles.iter().find(|p| **p != act[0]) {
                eprintln!("swapdex: '-' -> '{other}'");
                return Ok(Some(other.clone()));
            }
        }
        // Otherwise: the most recent switch to a profile that is neither
        // active now nor the destination of the newest switch (when the live
        // identity is unreadable, that newest destination IS the current
        // profile - excluding it keeps '-' from re-picking where you already
        // are).
        if let Some(prev) = last_switch_name_excluding(paths, &act, &profiles, sel) {
            eprintln!("swapdex: '-' -> '{prev}'");
            return Ok(Some(prev));
        }
        if act.len() > 1 {
            eprintln!(
                "swapdex: both profiles are active ({}) - '-' is ambiguous here; \
                 say which: swapdex use <{}>",
                act.join(", "),
                profiles.join("|")
            );
        } else {
            eprintln!(
                "swapdex: can't tell which profile '-' means yet. \
                 Pick one: swapdex use <{}>",
                profiles.join("|")
            );
        }
        return Ok(None);
    }
    if profiles.iter().any(|p| p == raw) {
        return Ok(Some(raw.to_string()));
    }
    let cands: Vec<&String> = profiles.iter().filter(|p| p.starts_with(raw)).collect();
    match cands.len() {
        1 => {
            let n = cands[0].clone();
            eprintln!("swapdex: '{raw}' matched profile '{n}'");
            Ok(Some(n))
        }
        // No prefix match: fall through so the normal "no profile" error runs.
        0 => Ok(Some(raw.to_string())),
        _ => {
            eprintln!(
                "swapdex: '{raw}' is ambiguous: {}",
                cands
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Ok(None)
        }
    }
}

/// The most recent `use`/`restore` timeline entry naming a profile that still
/// exists and is not in `exclude` - i.e. "the profile you were on before".
/// The destination of the NEWEST switch is also excluded (it is where you are
/// now, even when the live identity cannot be read), and `sel` scopes which
/// tools' events count.
fn last_switch_name_excluding(
    paths: &Paths,
    exclude: &[String],
    profiles: &[String],
    sel: Option<ToolSel>,
) -> Option<String> {
    let text = std::fs::read_to_string(paths.store_dir().join("timeline.jsonl")).ok()?;
    let mut events: Vec<(i64, String)> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !matches!(v["action"].as_str(), Some("use") | Some("restore")) {
            continue;
        }
        if let Some(tool) = v["tool"].as_str() {
            if !sel.map(|s| s.wants(tool)).unwrap_or(true) {
                continue;
            }
        }
        let (Some(ts), Some(name)) = (v["ts"].as_i64(), v["account"].as_str()) else {
            continue;
        };
        events.push((ts, name.to_string()));
    }
    // Where the newest switch went = where you are now; never "toggle" there.
    let newest = events
        .iter()
        .max_by_key(|(ts, _)| *ts)
        .map(|(_, n)| n.clone());
    let mut best: Option<(i64, String)> = None;
    for (ts, name) in events {
        if exclude.contains(&name)
            || newest.as_deref() == Some(name.as_str())
            || !profiles.contains(&name)
        {
            continue;
        }
        if best.as_ref().map(|(t, _)| ts >= *t).unwrap_or(true) {
            best = Some((ts, name.to_string()));
        }
    }
    best.map(|(_, n)| n)
}

/// The tool(s) the most recent switch (`use` or `restore`) touched, from the
/// timeline. Every tool of one invocation is written with the SAME ts
/// (append_timeline_at), so strict ts equality identifies the invocation.
/// None when no switch is on record - the caller falls back to every tool.
fn last_switch_tools(paths: &Paths) -> Option<Vec<String>> {
    let path = paths.store_dir().join("timeline.jsonl");
    let text = std::fs::read_to_string(path).ok()?;
    let mut events: Vec<(i64, String)> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !matches!(v["action"].as_str(), Some("use") | Some("restore")) {
            continue;
        }
        if let (Some(ts), Some(tool)) = (v["ts"].as_i64(), v["tool"].as_str()) {
            events.push((ts, tool.to_string()));
        }
    }
    let max_ts = events.iter().map(|(ts, _)| *ts).max()?;
    let mut tools: Vec<String> = events
        .into_iter()
        .filter(|(ts, _)| *ts == max_ts)
        .map(|(_, tool)| tool)
        .collect();
    tools.sort();
    tools.dedup();
    Some(tools)
}

/// "3m ago" / "2h ago" from a unix-nanos backup stamp.
fn age_line(stamp_nanos: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let secs = (now.saturating_sub(stamp_nanos) / 1_000_000_000) as u64;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
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
    // From here the snapshot EXISTS: a missing part or unparseable blob is an
    // UNREADABLE snapshot (surfaced as a marker), not silently "no data" - a
    // corrupt profile must be visible in `ls` before `use` trips over it.
    let unreadable = (None, None, Some("unreadable"));
    match tool {
        "claude-code" => {
            let (Some(cred_part), Some(oauth_part)) =
                (snap.part("credentials"), snap.part("oauth_account"))
            else {
                return Some(unreadable);
            };
            let (Ok(creds), Ok(oauth)) = (
                serde_json::from_slice::<Value>(cred_part.expose()),
                serde_json::from_slice::<Value>(oauth_part.expose()),
            ) else {
                return Some(unreadable);
            };
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
            let Some(auth_part) = snap.part("auth") else {
                return Some(unreadable);
            };
            let Ok(auth) = serde_json::from_slice::<Value>(auth_part.expose()) else {
                return Some(unreadable);
            };
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

/// Pad-or-truncate to `w` DISPLAY columns (CJK chars occupy two; counting
/// chars would shear the table); a longer value ends in one '…'.
fn fit(s: &str, w: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let n = UnicodeWidthStr::width(s);
    if n <= w {
        let mut out = String::from(s);
        out.extend(std::iter::repeat_n(' ', w - n));
        return out;
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    // Pad if the truncation landed a column short (a wide char didn't fit).
    out.extend(std::iter::repeat_n(' ', w.saturating_sub(used + 1)));
    out
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

pub fn ls(paths: &Paths, json: bool, names: bool) -> Result<i32> {
    let store = Store::open(paths)?;
    if names {
        // Bare names, one per line (store.list() is sorted) - for scripts and
        // the profile-name tab-completion snippet in the docs.
        for p in store.list() {
            println!("{}", p.name);
        }
        return Ok(0);
    }
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
    // Widths in CHARS (not bytes - non-ASCII names must not shear the table),
    // and content longer than the cap is truncated with '…' so one long row
    // cannot un-align every other. Full values stay available in `ls --json`.
    let name_w = rows
        .iter()
        .map(|r| unicode_width::UnicodeWidthStr::width(r.name.as_str()))
        .max()
        .unwrap_or(4)
        .clamp(4, 24);
    let ident_w = rows
        .iter()
        .map(|r| unicode_width::UnicodeWidthStr::width(r.ident.as_str()))
        .max()
        .unwrap_or(0)
        .clamp(0, 40);
    let mut saw_refreshable = false;
    let mut saw_unreadable = false;
    for r in &rows {
        let mark = if r.active { "* " } else { "  " };
        let warn = r.warn.map(|m| format!("  ({m})")).unwrap_or_default();
        saw_unreadable |= r.warn == Some("unreadable");
        saw_refreshable |= matches!(r.warn, Some("expired") | Some("stale"));
        println!(
            "{mark}{} {} [{}]{warn}",
            fit(&r.name, name_w),
            fit(&r.ident, ident_w),
            r.tools
        );
    }
    if saw_refreshable {
        println!(
            "  (expired/stale: re-run `swapdex add --update <name>` while logged in to refresh)"
        );
    }
    if saw_unreadable {
        println!(
            "  (unreadable: the saved snapshot is corrupt - log in to that account and \
             re-save it with `swapdex add <name> --update`)"
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

/// One compact line for shell prompts / statuslines: `claude:work codex:personal`.
/// The value per tool is the matched profile name, falling back to the email.
/// None when nothing is logged in (or nothing is readable).
pub fn short_line(paths: &Paths) -> Option<String> {
    let store = Store::open(paths).ok()?;
    let parts: Vec<String> = adapters::all()
        .iter()
        .filter_map(|a| {
            let id = a.identity(paths).ok().flatten()?;
            let tool = match a.name() {
                "claude-code" => "claude",
                t => t,
            };
            let who = matched_profile_name(&store, a.name(), &id.account_id)
                .or(id.email)
                .unwrap_or_else(|| "?".into());
            Some(format!("{tool}:{who}"))
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

pub fn status(paths: &Paths, json: bool, short: bool) -> Result<i32> {
    if short {
        println!("{}", short_line(paths).unwrap_or_default());
        return Ok(0);
    }
    let store = Store::open(paths)?;
    if json {
        let rows: Vec<Value> = adapters::all()
            .iter()
            .map(|adapter| {
                let tool = adapter.name();
                // Stable shape: every key present on every row, null when
                // unknown, so `jq .[].email` never needs guards.
                match adapter.identity(paths) {
                    Err(_) => serde_json::json!({
                        "tool": tool, "logged_in": false, "unreadable": true,
                        "email": null, "tier": null, "profile": null, "expired": null,
                    }),
                    Ok(None) => serde_json::json!({
                        "tool": tool, "logged_in": false, "unreadable": false,
                        "email": null, "tier": null, "profile": null, "expired": null,
                    }),
                    Ok(Some(id)) => serde_json::json!({
                        "tool": tool,
                        "logged_in": true,
                        "unreadable": false,
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
        match adapter.identity(paths) {
            Err(_) => println!(
                "{tool}: login file unreadable - `swapdex use <profile>` can replace it \
                 (or log in again in the tool)"
            ),
            Ok(None) => match macos_keychain_note(paths, tool) {
                Some(note) => println!("{tool}: not manageable - {note}"),
                None => println!("{tool}: not logged in"),
            },
            Ok(Some(id)) => {
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

/// `ui` - a numbered interactive picker: see every profile (active marked from
/// the live login), type a number, switch. Plain Enter cancels. Deliberately
/// stdin-only (no raw-mode/TUI crate pulls a socket library into the graph),
/// and the switch itself goes through the exact same `use` path - a human
/// picking a number IS the explicit `swapdex use <name>`.
pub fn ui(paths: &Paths) -> Result<i32> {
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal() || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
    if !tty {
        eprintln!("swapdex: `ui` is interactive and needs a terminal (try `swapdex use <name>`)");
        return Ok(2);
    }
    let store = Store::open(paths)?;
    let profiles = store.list();
    if profiles.is_empty() {
        println!("No accounts saved yet.");
        println!("  guided setup:  swapdex setup");
        return Ok(0);
    }
    let active = active_by_tool(&store, paths);
    let color = crate::util::color_enabled();
    println!();
    for (i, p) in profiles.iter().enumerate() {
        let (email, tier, marker) = profile_summary(&store, &p.name, &p.tools);
        let at: Vec<&str> = active
            .iter()
            .filter(|(_, n)| n == &p.name)
            .map(|(t, _)| *t)
            .collect();
        let star = if at.is_empty() { "  " } else { "* " };
        let ident = identity_column(email, tier);
        let warn = marker.map(|m| format!("  ({m})")).unwrap_or_default();
        let line = format!(
            "  {}) {star}{} {} [{}]{warn}",
            i + 1,
            fit(&p.name, 16),
            fit(&ident, 32),
            p.tools.join(", ")
        );
        if color && !at.is_empty() {
            println!("\x1b[1m{line}\x1b[0m");
        } else {
            println!("{line}");
        }
    }
    if let Some(line) = crate::session_link::status_line(paths) {
        println!("\n  {line}");
    }
    println!();
    loop {
        let Some(ans) = prompt(
            &format!("switch to [1-{}] (Enter cancels): ", profiles.len()),
            "",
        ) else {
            println!("cancelled - nothing switched.");
            return Ok(0);
        };
        if ans.is_empty() || ans.eq_ignore_ascii_case("q") {
            println!("cancelled - nothing switched.");
            return Ok(0);
        }
        match ans.parse::<usize>() {
            Ok(n) if (1..=profiles.len()).contains(&n) => {
                let name = profiles[n - 1].name.clone();
                println!();
                return use_account(paths, &name, None, false);
            }
            _ => {
                println!(
                    "  pick a number between 1 and {} (Enter cancels)",
                    profiles.len()
                );
            }
        }
    }
}

/// `doctor` - local-only health check with a remedy per finding. Exit 0 when
/// healthy, 9 when any problem was found (scripts can gate on it). Checks the
/// store, every saved snapshot, both live logins, backups, and the CLIs on
/// PATH - and never touches the network.
pub fn doctor(paths: &Paths) -> Result<i32> {
    use std::os::unix::fs::PermissionsExt;
    // Stat BEFORE Store::open, which self-heals the mode to 0700 - otherwise
    // the permission check below could never observe a problem.
    let pre_mode = std::fs::metadata(paths.store_dir())
        .ok()
        .map(|m| m.permissions().mode() & 0o777);
    let store = Store::open(paths)?;
    let mut problems = 0u32;
    let color = crate::util::color_enabled();
    let mut report = |label: &str, ok: bool, msg: String| {
        let verdict = match (ok, color) {
            (true, true) => "\x1b[32mok\x1b[0m".to_string(),
            (false, true) => "\x1b[31mproblem\x1b[0m".to_string(),
            (true, false) => "ok".to_string(),
            (false, false) => "problem".to_string(),
        };
        println!("{label:<13} {verdict} - {msg}");
        if !ok {
            problems += 1;
        }
    };

    // Store directory. Store::open already tightened it to 0700; report what
    // it FOUND (pre_mode), or "ok" would paper over a store that sat
    // group-readable until this very run.
    let sd = paths.store_dir();
    let profiles = store.list();
    let count = format!(
        "{} profile{}",
        profiles.len(),
        if profiles.len() == 1 { "" } else { "s" }
    );
    match (pre_mode, std::fs::metadata(&sd)) {
        (Some(m), Ok(now)) if m & 0o077 != 0 && now.permissions().mode() & 0o077 == 0 => report(
            "store",
            true,
            format!("was mode {m:03o} - tightened to 0700 just now; {count}"),
        ),
        (_, Ok(now)) if now.permissions().mode() & 0o077 != 0 => report(
            "store",
            false,
            format!(
                "directory is group/world-accessible; run `chmod 700 {}`",
                crate::util::redact_path(&sd.display().to_string())
            ),
        ),
        (_, Ok(_)) => report("store", true, format!("0700, {count}")),
        (_, Err(e)) => report("store", false, format!("cannot stat store dir: {e}")),
    }

    // Live logins per tool.
    for adapter in adapters::all() {
        let tool = adapter.name();
        match adapter.identity(paths) {
            Ok(Some(id)) => {
                let saved = matched_profile_name(&store, tool, &id.account_id)
                    .map(|n| format!("profile '{n}'"))
                    .unwrap_or_else(|| "not saved - `swapdex add <name>` keeps it".into());
                report(
                    tool,
                    true,
                    format!("live login {} ({saved})", identity_line(&id)),
                );
            }
            Ok(None) => match macos_keychain_note(paths, tool) {
                Some(note) => report(tool, true, format!("not manageable - {note}")),
                None => report(tool, true, "not logged in".into()),
            },
            Err(_) => report(
                tool,
                false,
                "live login file unreadable; `swapdex use <profile>` can replace it, \
                 or log in again in the tool"
                    .into(),
            ),
        }
    }

    // .claude.json permissions (holds account PII).
    if let Ok(meta) = std::fs::metadata(paths.claude_config_json()) {
        if meta.permissions().mode() & 0o077 != 0 {
            report(
                "claude-config",
                false,
                format!(
                    "{} is group/world-readable; run `chmod 600` on it",
                    crate::util::redact_path(&paths.claude_config_json().display().to_string())
                ),
            );
        }
    }

    // Every saved snapshot must parse.
    for p in &profiles {
        for tool in &p.tools {
            match profile_detail(&store, &p.name, tool) {
                Some((_, _, Some("unreadable"))) => report(
                    &format!("profile:{}", p.name),
                    false,
                    format!(
                        "{tool} snapshot unreadable; log in to that account and run \
                         `swapdex add {} --tool {tool} --update`",
                        p.name
                    ),
                ),
                // The precondition matters: `add --update` snapshots whatever
                // is LIVE, so without "log in to that account first" the
                // remedy would overwrite this profile with the wrong account.
                Some((_, _, Some(m))) => report(
                    &format!("profile:{}", p.name),
                    true,
                    format!(
                        "{tool} snapshot {m} - log in to that account and run \
                         `swapdex add {} --tool {tool} --update`",
                        p.name
                    ),
                ),
                _ => {}
            }
        }
    }

    // Backups: newest intact per tool (load_backup already skips torn ones).
    let mut kept = Vec::new();
    for tool in ["claude-code", "codex"] {
        if let Ok(Some((stamp, _))) = store.load_backup(tool) {
            kept.push(format!("{tool} (newest {})", age_line(stamp)));
        }
    }
    if kept.is_empty() {
        report(
            "backups",
            true,
            "none yet (one is taken on every `use`; `swapdex restore` brings it back)".into(),
        );
    } else {
        report("backups", true, format!("intact - {}", kept.join(", ")));
    }

    // CLIs on PATH - informational (a codex-only user is not "broken").
    let mut found = Vec::new();
    for cli in ["claude", "codex"] {
        if command_exists(cli) {
            found.push(cli);
        }
    }
    report(
        "tools",
        true,
        if found.is_empty() {
            "neither `claude` nor `codex` found on PATH".into()
        } else {
            format!("on PATH: {}", found.join(", "))
        },
    );

    if problems > 0 {
        println!(
            "\n{problems} problem{} found - each line above ends with its fix.",
            if problems == 1 { "" } else { "s" }
        );
        return Ok(9);
    }
    println!("\neverything looks healthy.");
    Ok(0)
}

pub fn rm(paths: &Paths, name: &str, yes: bool) -> Result<i32> {
    if let Some(c) = reject_bad_name(name) {
        return Ok(c);
    }
    let store = Store::open(paths)?;
    // Existence first - never ask "delete 'ghost'?" about a profile that
    // does not exist.
    if !store.list().iter().any(|p| p.name == name) {
        eprintln!("swapdex: no profile named '{name}'");
        return Ok(5);
    }
    if !yes {
        // On a terminal, just ask; --yes stays for scripts (and remains the
        // only path when stdin is not a tty, exit 7 as documented).
        use std::io::IsTerminal;
        let tty =
            std::io::stdin().is_terminal() || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
        if !tty {
            eprintln!(
                "swapdex: `rm {name}` deletes the saved profile. Re-run with --yes to confirm."
            );
            return Ok(7);
        }
        if !yes_no(
            &format!("delete saved profile '{name}'? The live login stays. [y/N]: "),
            false,
        ) {
            println!("kept '{name}'.");
            return Ok(0);
        }
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
    if let Some(c) = reject_bad_name(new).or_else(|| reject_reserved_name(new)) {
        return Ok(c);
    }
    let store = Store::open(paths)?;
    // Take the switch lock like every other store mutation, and make the
    // collision a first-class "already exists" (6) rather than a hard error -
    // a script must be able to tell "pick another name" from "disk broke".
    let _lock = match store.lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("swapdex: another swapdex is mid-switch; try again");
            return Ok(4);
        }
    };
    if store.list().iter().any(|p| p.name == new) {
        eprintln!("swapdex: a profile named '{new}' already exists");
        return Ok(6);
    }
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
            // stderr + exit 3, same as the codex path: `login x && ...` in a
            // script must not proceed as if a login was saved.
            eprintln!("swapdex: Claude Code isn't on your PATH. Install it, then:");
            eprintln!("  1) run `claude` and complete the login");
            eprintln!("  2) then:  swapdex add {name} --tool claude");
            return Ok(3);
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
        return add(paths, Some(name), Some(ToolSel::Claude), true);
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
    add(paths, Some(name), Some(ToolSel::Codex), true)
}

/// Ask a question and read a trimmed line; empty input yields `default`.
/// Ask on stdout, read a line. `None` means the input stream ENDED (Ctrl-D or
/// a closed pipe) - callers must stop asking, or the wizard would spin forever
/// re-prompting into a stream that can never answer.
fn prompt(question: &str, default: &str) -> Option<String> {
    use std::io::Write;
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => return None, // EOF or broken stream
        Ok(_) => {}
    }
    let t = line.trim();
    Some(if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    })
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
    // EOF answers "no": never take an irreversible step on a dead stream.
    let Some(a) = prompt(question, if default_yes { "y" } else { "n" }) else {
        return false;
    };
    matches!(a.to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Ask for a profile name, re-prompting until it is valid (or the user skips).
/// An existing name asks whether to replace it. Returns None on skip.
fn ask_name(store: &Store, question: &str, default: &str) -> Option<String> {
    loop {
        let ans = prompt(question, default)?; // EOF -> skip, never loop
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
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
            println!(
                "session data unavailable (install sessionwiki to see sessions grouped by account)"
            );
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
