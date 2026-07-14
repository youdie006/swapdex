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
/// The `--tool` flag value for a tool name (claude-code -> claude).
fn pretty_tool_flag(tool: &str) -> &str {
    if tool == "claude-code" {
        "claude"
    } else {
        tool
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

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
    Gemini,
    Antigravity,
    /// Every tool (the default when --tool is omitted)
    #[value(alias = "both")]
    All,
}

impl ToolSel {
    fn wants(self, tool: &str) -> bool {
        match self {
            ToolSel::Claude => tool == "claude-code",
            ToolSel::Codex => tool == "codex",
            ToolSel::Gemini => tool == "gemini",
            ToolSel::Antigravity => tool == "antigravity",
            ToolSel::All => true,
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
    matches!(
        sel,
        Some(ToolSel::Claude)
            | Some(ToolSel::Codex)
            | Some(ToolSel::Gemini)
            | Some(ToolSel::Antigravity)
    )
}

/// On macOS, Claude Code keeps its OAuth login in the Keychain rather than in
/// `~/.claude/.credentials.json`, so swapdex sees "not logged in" even when a
/// login exists. When the config file proves a login is present, explain that
/// instead of gaslighting the user. (`cfg!` keeps this type-checked on Linux.)
fn macos_keychain_note(_paths: &Paths, _tool: &str) -> Option<&'static str> {
    // Claude Code on macOS is now supported: the adapter reads and writes the
    // login Keychain via `security`. So there is no longer anything to skip or
    // warn about - this returns None everywhere and the old skip branches are
    // inert. (Kept as a single seam in case a future tool needs a similar
    // platform note.)
    None
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
        "gemini" => {
            let v: Value = serde_json::from_slice(snap.part("oauth")?.expose()).ok()?;
            crate::adapters::gemini_jwt_claim(v["id_token"].as_str(), "sub")
        }
        "antigravity" => {
            let v: Value = serde_json::from_slice(snap.part("token")?.expose()).ok()?;
            let fp = crate::adapters::antigravity_fingerprint(&v);
            (!fp.is_empty()).then_some(fp)
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

/// Every profile holding this tool+account - refresh targets when the live
/// login (with its freshest, possibly rotated tokens) is switched away.
fn matching_profile_names(store: &Store, tool: &str, live_id: &str) -> Vec<String> {
    if live_id.is_empty() {
        return Vec::new();
    }
    store
        .list()
        .into_iter()
        .filter(|p| {
            p.tools.iter().any(|t| t == tool)
                && profile_account_id(store, &p.name, tool).as_deref() == Some(live_id)
        })
        .map(|p| p.name)
        .collect()
}

/// Reject a profile name that could escape the store (path traversal). Returns
/// the exit code to use if invalid.
fn reject_bad_name(name: &str) -> Option<i32> {
    if crate::store::valid_profile_name(name) {
        None
    } else {
        eprintln!(
            "swapdex: invalid profile name '{name}' (1-64 bytes, not all spaces; \
             no '/', '\\', leading '.', or control chars)"
        );
        Some(2)
    }
}

/// Additionally reject "-" where a profile is CREATED (`use -` toggles, so a
/// new profile must never take that name; a legacy one stays manageable).
fn reject_reserved_name(name: &str) -> Option<i32> {
    if name == "-" {
        eprintln!("swapdex: '-' is reserved (`swapdex use -` toggles to the previous profile)");
        Some(2)
    } else if name.trim().is_empty() {
        // CREATION-time only (like '-'): a legacy all-whitespace profile from
        // 0.2.x must stay rm-able/renamable after an upgrade.
        eprintln!("swapdex: a profile name cannot be only whitespace");
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
        Err(crate::store::LockError::Busy) => {
            eprintln!(
                "swapdex: another swapdex is busy (a switch, or a `swapdex login` waiting \
                 for a sign-in). Finish or close it, then retry."
            );
            return Ok(4);
        }
        Err(crate::store::LockError::Unwritable(e)) => {
            eprintln!(
                "swapdex: the store is not writable ({e}) - check permissions/mount of \
                 the store directory"
            );
            return Ok(4);
        }
    };
    let mut saved = Vec::new();
    let mut skipped = Vec::new();
    let mut capture_failed: Vec<&str> = Vec::new();
    let mut declined: Vec<&str> = Vec::new(); // repoint prompt answered No
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
        if update {
            // Updating must not silently REPOINT the profile to a different
            // account - that changes what the name means. Same-account
            // updates (the documented stale-token refresh) pass through.
            let stored_id = profile_account_id(&store, name, tool).filter(|s| !s.is_empty());
            let live_id = adapter
                .identity(paths)
                .ok()
                .flatten()
                .map(|i| i.account_id)
                .filter(|s| !s.is_empty());
            if let (Some(stored), Some(live)) = (&stored_id, &live_id) {
                if stored != live {
                    use std::io::IsTerminal;
                    let tty = std::io::stdin().is_terminal()
                        || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
                    let msg = format!(
                        "profile '{name}' holds a different account for {tool} \
                         than the one you're logged into"
                    );
                    if !tty {
                        eprintln!("swapdex: {msg}.");
                        eprintln!(
                            "  keep both: swapdex add <new-name> --tool {}  |  really \
                             repoint: swapdex rm {name} && swapdex add {name}",
                            pretty_tool_flag(tool)
                        );
                        return Ok(7);
                    }
                    if !yes_no(
                        &format!("{msg}. Repoint '{name}' to the current login? [y/N]: "),
                        false,
                    ) {
                        println!("skipped {tool}.");
                        declined.push(tool);
                        continue;
                    }
                }
            }
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
        let snap = match adapter.capture(paths) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("swapdex: {tool}: could not read the live login ({e:#}) - skipped");
                capture_failed.push(tool);
                continue;
            }
        };
        store.save(name, &snap)?;
        saved.push(tool);
    }
    if saved.is_empty() {
        if !declined.is_empty() {
            // The user was logged in but declined to repoint - nothing wrong,
            // nothing saved. NOT "not logged in" (exit 3 would be a lie).
            println!(
                "nothing saved for {} (you declined the repoint).",
                declined.join(", ")
            );
            return Ok(0);
        }
        if !skipped.is_empty() {
            eprintln!(
                "swapdex: profile '{name}' already has {}; pass --update to replace",
                skipped.join(", ")
            );
            return Ok(6);
        }
        if !capture_failed.is_empty() {
            // present() said the login IS there but capture failed - a corrupt
            // or unreadable live login (a hand-edited ~/.claude.json with a
            // JSON syntax error is the common one), NOT "not logged in". The
            // per-tool error above carries the fix; this is a hard error (1),
            // never exit 3 (which would send the user to re-log-in in vain).
            eprintln!(
                "swapdex: nothing saved - the live login for {} is present but could not be \
                 read (see the error above)",
                capture_failed.join(", ")
            );
            return Ok(1);
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
    if !capture_failed.is_empty() {
        eprintln!(
            "swapdex: {} tool(s) could not be read and were NOT saved: {}",
            capture_failed.len(),
            capture_failed.join(", ")
        );
    }
    if name.contains(char::is_whitespace) {
        println!(
            "note: the name has spaces - quote it in later commands (`swapdex use \"{name}\"`)"
        );
    }
    Ok(if capture_failed.is_empty() { 0 } else { 1 })
}

pub fn use_account(paths: &Paths, name: &str, sel: Option<ToolSel>, dry_run: bool) -> Result<i32> {
    use_account_inner(paths, name, sel, dry_run, false, None)
}

/// `open`: after a successful switch, exec the tool (the --open flag; needs an
/// explicit --tool so there is never a guess about WHICH conversation opens).
pub fn use_account_open(
    paths: &Paths,
    name: &str,
    sel: Option<ToolSel>,
    dir: Option<&std::path::Path>,
) -> Result<i32> {
    if !is_explicit(sel) {
        eprintln!("swapdex: --open needs --tool <claude|codex|gemini|antigravity> so it knows what to launch");
        return Ok(2);
    }
    if let Some(d) = dir {
        if !d.is_dir() {
            eprintln!("swapdex: --dir is not a directory: {}", d.display());
            return Ok(2);
        }
    }
    use_account_inner(paths, name, sel, false, true, dir)
}

fn use_account_inner(
    paths: &Paths,
    name: &str,
    sel: Option<ToolSel>,
    dry_run: bool,
    open: bool,
    open_dir: Option<&std::path::Path>,
) -> Result<i32> {
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
        Err(crate::store::LockError::Busy) => {
            eprintln!(
                "swapdex: another swapdex is busy (a switch, or a `swapdex login` waiting \
                 for a sign-in). Finish or close it, then retry."
            );
            return Ok(4);
        }
        Err(crate::store::LockError::Unwritable(e)) => {
            eprintln!(
                "swapdex: the store is not writable ({e}) - check permissions/mount of \
                 the store directory"
            );
            return Ok(4);
        }
    };
    // A typo must be ONE line, not four "left unchanged" notes implying the
    // profile exists but lacks those tools.
    if !store.list().iter().any(|p| p.name == name) {
        eprintln!("swapdex: no profile named '{name}'");
        return Ok(5);
    }
    let mut matched = 0; // profile had a snapshot for this tool
    let mut changed = 0; // an actual switch was written
    let mut failed: Vec<&str> = Vec::new(); // tools whose switch errored

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
    let switch_inv = now_nanos();
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
            // Still a sync point: the live login IS this profile's account
            // and its tokens may have rotated since the last save. No backup
            // and no timeline event - nothing is switching.
            if !dry_run {
                if let (Ok(snap), Some(id)) = (adapter.capture(paths), &live_id) {
                    for pname in matching_profile_names(&store, tool, id) {
                        store.save(&pname, &snap)?;
                    }
                }
            }
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
                    // Refresh tokens ROTATE while an account is in use, so a
                    // profile snapshot goes stale the moment you work on that
                    // account. Write the live capture (the freshest known
                    // tokens) back into every profile holding this account -
                    // otherwise switching back later restores a refresh token
                    // the provider may have already revoked.
                    if let Some(id) = &live_id {
                        for pname in matching_profile_names(&store, tool, id) {
                            store.save(&pname, &live_snap)?;
                        }
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
                Err(e) if live.is_some() => {
                    // identity() SUCCEEDED but capture() failed: the login is
                    // valid and recoverable, only a sibling file is corrupt (a
                    // hand-edited ~/.claude.json, or a Gemini
                    // google_accounts.json). Applying now would OVERWRITE a
                    // recoverable login with NO backup - a lost login. Refuse
                    // for this tool (the others still switch) and point at the
                    // repair.
                    eprintln!(
                        "swapdex: {tool}: the current login is present but could not be backed \
                         up ({e:#}) - refusing to overwrite it without a backup. Repair the file \
                         named above (or re-login), then retry."
                    );
                    failed.push(tool);
                    continue;
                }
                Err(e) => eprintln!(
                    // identity() ALSO failed: the live login is genuinely
                    // broken (its primary credential is unparseable), so there
                    // is nothing recoverable to preserve - switching in a good
                    // profile IS the fix. Proceed, warning about the skipped
                    // backup.
                    "swapdex: note - the current {tool} login could not be read ({e:#}); \
                     switching without a backup of it"
                ),
            }
        }
        if let Err(e) = adapter.apply(paths, &target) {
            // Do NOT abort the whole multi-tool switch: the other tools can
            // still switch; a summary at the end says what failed.
            eprintln!(
                "swapdex: {tool}: switch failed - {:#}\n  (if the error is about the \
                 SNAPSHOT: log in to that account and re-save with `swapdex add {name} \
                 --tool {} --update`)",
                e,
                pretty_tool_flag(tool)
            );
            failed.push(tool);
            continue;
        }
        store.append_timeline_inv(tool, name, "use", switch_ts, switch_inv)?;
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
    if !failed.is_empty() {
        eprintln!(
            "swapdex: {} tool(s) failed to switch ({}); the tools above did switch - \
             `swapdex restore` undoes this switch entirely",
            failed.len(),
            failed.join(", ")
        );
        return Ok(1);
    }
    if open {
        if let Some(adapter) = selected_adapters(sel).into_iter().next() {
            let tool = adapter.name();
            println!("opening {}...", pretty_tool(tool));
            return Err(exec_tool(tool, open_dir));
        }
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
        Err(crate::store::LockError::Busy) => {
            eprintln!(
                "swapdex: another swapdex is busy (a switch, or a `swapdex login` waiting \
                 for a sign-in). Finish or close it, then retry."
            );
            return Ok(4);
        }
        Err(crate::store::LockError::Unwritable(e)) => {
            eprintln!(
                "swapdex: the store is not writable ({e}) - check permissions/mount of \
                 the store directory"
            );
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
    let restore_inv = now_nanos();
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
                Ok(live_snap) => {
                    store.backup(&live_snap)?;
                    // Same rotation invariant as `use`: the outgoing live
                    // login carries this account's freshest tokens.
                    if let Some(id) = &live_id {
                        for pname in matching_profile_names(&store, tool, id) {
                            store.save(&pname, &live_snap)?;
                        }
                    }
                }
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
        store.append_timeline_inv(tool, &event_name, "restore", restore_ts, restore_inv)?;
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
    let mut events: Vec<(i64, String, String)> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !matches!(v["action"].as_str(), Some("use") | Some("restore")) {
            continue;
        }
        if let (Some(ts), Some(tool)) = (v["ts"].as_i64(), v["tool"].as_str()) {
            let inv = v["inv"].as_str().unwrap_or("").to_string();
            events.push((ts, inv, tool.to_string()));
        }
    }
    // Group by the last event's INVOCATION id when it has one - whole-second
    // ts equality collides when two separate invocations run inside one
    // second. Legacy events (no inv) fall back to ts grouping.
    let (last_ts, last_inv) = events
        .iter()
        .map(|(ts, inv, _)| (*ts, inv.clone()))
        .next_back()?;
    let mut tools: Vec<String> = events
        .into_iter()
        .filter(|(ts, inv, _)| {
            if last_inv.is_empty() {
                *ts == last_ts && inv.is_empty()
            } else {
                *inv == last_inv
            }
        })
        .map(|(_, _, tool)| tool)
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
/// (email, tier, marker). marker is "stale" (a login snapshot older than
/// STALE_DAYS whose refresh token may
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
            // Claude access tokens live ~1h and Claude Code refreshes them
            // silently with the refresh token, so "expired" the moment the
            // access token lapses is pure noise (this was the constant
            // "expired" spam). Only flag a snapshot whose access token is
            // ANCIENT (>30 days) - by then the refresh token itself may be
            // revoked. Same rule as Codex / Gemini / Antigravity.
            let marker = creds["claudeAiOauth"]["expiresAt"]
                .as_i64()
                .filter(|ms| now_ms() - ms > STALE_DAYS * 86400 * 1000)
                .map(|_| "stale");
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
        "gemini" => {
            let oauth: Value = serde_json::from_slice(snap.part("oauth")?.expose()).ok()?;
            let email = snap
                .part("accounts")
                .and_then(|a| serde_json::from_slice::<Value>(a.expose()).ok())
                .and_then(|v| v["active"].as_str().map(String::from))
                .or_else(|| crate::adapters::gemini_jwt_claim(oauth["id_token"].as_str(), "email"));
            // Gemini access tokens live ~1h and the CLI refreshes them
            // silently, so "expired right now" is noise. Meaningful signal:
            // a snapshot whose expiry is ANCIENT was refreshed long ago and
            // its refresh token may be revoked - same idea as codex's stale.
            let marker = oauth["expiry_date"]
                .as_i64()
                .filter(|ms| now_ms() - ms > STALE_DAYS * 86400 * 1000)
                .map(|_| "stale");
            Some((email, None, marker))
        }
        "antigravity" => {
            let v: Value = serde_json::from_slice(snap.part("token")?.expose()).ok()?;
            // A snapshot whose token expiry is ancient was refreshed long ago;
            // its refresh token may be revoked - same idea as codex's stale.
            let marker = v["token"]["expiry"]
                .as_str()
                .and_then(crate::session_link::rfc3339_to_secs)
                .filter(|&secs| now_ms() / 1000 - secs > STALE_DAYS * 86400)
                .map(|_| "stale");
            Some((None, v["auth_method"].as_str().map(String::from), marker))
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
    // Adapter order (Claude first), NOT the store's alphabetical order:
    // antigravity sorts first alphabetically and its auth_method ("consumer")
    // would mask claude's real plan tier ("max") on multi-tool profiles.
    for a in adapters::all() {
        let t = a.name();
        if !tools.iter().any(|x| x == t) {
            continue;
        }
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
    let real_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let tty = real_tty || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
    if !tty {
        eprintln!("swapdex: `ui` is interactive and needs a terminal (try `swapdex use <name>`)");
        return Ok(2);
    }
    // A real terminal gets the full-screen picker; the pipe-driven path (tests,
    // SWAPDEX_ASSUME_TTY) keeps the plain numbered prompt below. TERM=dumb /
    // empty (Emacs shell, some CI) cannot render ANSI - crossterm never checks
    // TERM, so we do.
    let dumb = std::env::var("TERM")
        .map(|t| t.is_empty() || t == "dumb")
        .unwrap_or(true);
    if real_tty && !dumb {
        return ui_tui(paths);
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
                // Timeline state BEFORE the switch below appends its own
                // event - otherwise the first-ever switch would skip the
                // fallback written exactly for it.
                let first_time = crate::session_link::read_timeline(paths).is_empty();
                let rc = use_account(paths, &name, None, false)?;
                if rc == 0 {
                    ui_session_hints(paths, &name, first_time)?;
                }
                return Ok(rc);
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

/// One session row for the post-switch menu, whatever the source.
pub(crate) enum MenuSession {
    Wiki(crate::session_link::RecentSession),
    Native(crate::native_sessions::NativeSession),
}

impl MenuSession {
    pub(crate) fn describe(&self) -> (String, i64, String, String) {
        match self {
            MenuSession::Wiki(s) => (
                s.id.chars().take(6).collect(),
                s.started,
                s.tool.clone(),
                s.title.clone(),
            ),
            MenuSession::Native(s) => (
                s.id.chars().take(6).collect(),
                s.started,
                s.tool.to_string(),
                s.title.clone(),
            ),
        }
    }
}

/// Recent sessions for the just-switched profile - sessionwiki when present
/// (cross-tool, richer), the tools' own on-disk stores otherwise. Never
/// requires sessionwiki (real-use feedback).
pub(crate) fn recent_menu_sessions(
    paths: &Paths,
    name: &str,
    first_time: bool,
    n: usize,
) -> (Vec<MenuSession>, String) {
    // sessionwiki path (attributed, then honest any-account fallback).
    if let Some(r) = crate::session_link::recent_sessions_for(paths, name, n) {
        if !r.is_empty() {
            return (
                r.into_iter().map(MenuSession::Wiki).collect(),
                format!("recent sessions on '{name}' (sessionwiki):"),
            );
        }
        // No sessions attributed to this account: still show recent ones so
        // the menu is useful (you can resume any). Attribution is best-effort;
        // an empty menu is worse than a broad one.
        if let Some(any) = crate::session_link::recent_sessions_any(n) {
            if !any.is_empty() {
                let label = if first_time {
                    "recent sessions (any account - attribution starts with your first switch):"
                } else {
                    "recent sessions (any account):"
                };
                return (
                    any.into_iter().map(MenuSession::Wiki).collect(),
                    label.to_string(),
                );
            }
        }
        // sessionwiki is present but returned NOTHING (installed yet never
        // `sessionwiki sync`ed, or a genuinely empty index). Do NOT stop here -
        // fall through to the native reader so the real on-disk sessions still
        // show, instead of a blank menu that hides sessions the user can see.
    }
    // Native path: straight from ~/.claude and ~/.codex.
    let events = crate::session_link::read_timeline(paths);
    let all = crate::native_sessions::recent(paths, n * 4);
    let mine: Vec<crate::native_sessions::NativeSession> = all
        .iter()
        .filter(|s| {
            crate::session_link::attribute(&events, s.tool, s.started).as_deref() == Some(name)
        })
        .map(|s| crate::native_sessions::NativeSession {
            tool: s.tool,
            id: s.id.clone(),
            title: s.title.clone(),
            cwd: s.cwd.clone(),
            started: s.started,
        })
        .take(n)
        .collect();
    if !mine.is_empty() {
        return (
            mine.into_iter().map(MenuSession::Native).collect(),
            format!("recent sessions on '{name}':"),
        );
    }
    let any: Vec<MenuSession> = all.into_iter().take(n).map(MenuSession::Native).collect();
    if !any.is_empty() {
        let label = if first_time {
            "recent sessions (any account - attribution starts with your first switch):"
        } else {
            "recent sessions (any account):"
        };
        return (any, label.to_string());
    }
    (Vec::new(), String::new())
}

/// Exec the resume for a picked menu session (never returns on success).
fn exec_menu_resume(s: &MenuSession) -> anyhow::Error {
    match s {
        MenuSession::Wiki(w) => {
            println!("opening session {} via sessionwiki...", w.id);
            exec_sessionwiki_resume(&w.id)
        }
        MenuSession::Native(nat) => {
            println!("resuming {} session {}...", pretty_tool(nat.tool), nat.id);
            crate::native_sessions::exec_resume(nat)
        }
    }
}

/// Post-switch continuity: recent sessions of the picked account + the
/// numbered resume handoff. Shared by the numbered picker and the TUI.
fn ui_session_hints(paths: &Paths, name: &str, first_time: bool) -> Result<()> {
    // `first_time` is captured by the CALLER before the switch writes its own
    // timeline event.
    let (recent, label) = recent_menu_sessions(paths, name, first_time, 3);
    // Offer "open a NEW X" only for tools THIS profile actually holds: launching
    // a tool that was NOT switched would open the user's unrelated live account
    // (the plain-picker twin of the TUI's new_conv_for filtering).
    let ptools = profile_tools(paths, name);
    let choices: Vec<(&str, &str, &str)> = [
        ("c", "claude-code", "claude"),
        ("x", "codex", "codex"),
        ("g", "gemini", "gemini"),
        ("a", "antigravity", "agy"),
    ]
    .into_iter()
    .filter(|(_, tool, _)| ptools.iter().any(|t| t == tool))
    .collect();
    let new_hint = if choices.is_empty() {
        String::new()
    } else {
        let keys = choices
            .iter()
            .map(|(k, _, p)| format!("{k} new {p}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(", {keys}")
    };
    let pick = |ans: &str| -> Option<&'static str> {
        launch_letter(ans).filter(|t| choices.iter().any(|(_, ct, _)| ct == t))
    };
    if !recent.is_empty() {
        println!("\n{label}");
        for (i, s) in recent.iter().enumerate() {
            let (id6, started, tool, title) = s.describe();
            let age = age_line((started.max(0) as u128) * 1_000_000_000);
            let line = format!(
                "  {}) {id6}  {:>7}  {}  {}",
                i + 1,
                age,
                fit(&format!("[{tool}]"), 13),
                fit(&title, 44)
            );
            println!("{}", line.trim_end());
        }
        if let Some(ans) = prompt(
            &format!(
                "open: [1-{}] resume that session{new_hint}, Enter skips: ",
                recent.len()
            ),
            "",
        ) {
            if let Ok(k) = ans.parse::<usize>() {
                if (1..=recent.len()).contains(&k) {
                    return Err(exec_menu_resume(&recent[k - 1]));
                }
            }
            if let Some(tool) = pick(&ans) {
                return Err(launch_in_folder(tool));
            }
        }
    } else if !choices.is_empty() {
        if let Some(ans) = prompt(&format!("open now?{new_hint} (Enter skips): "), "") {
            if let Some(tool) = pick(&ans) {
                return Err(launch_in_folder(tool));
            }
        }
    }
    Ok(())
}

/// The tools a saved profile holds (empty if the profile is unknown).
fn profile_tools(paths: &Paths, name: &str) -> Vec<String> {
    Store::open(paths)
        .ok()
        .and_then(|s| {
            s.list()
                .into_iter()
                .find(|p| p.name == name)
                .map(|p| p.tools)
        })
        .unwrap_or_default()
}

/// Ask for the project folder (conversations are per-directory), then exec.
/// Enter keeps the current directory.
fn launch_in_folder(tool: &str) -> anyhow::Error {
    let dir = prompt("folder to open in [current dir]: ", "")
        .filter(|d| !d.is_empty())
        .map(|d| {
            if d == "~" {
                if let Some(home) = dirs::home_dir() {
                    return home;
                }
            }
            if let Some(rest) = d.strip_prefix("~/") {
                if let Some(home) = dirs::home_dir() {
                    return home.join(rest);
                }
            }
            std::path::PathBuf::from(d)
        });
    if let Some(d) = &dir {
        if !d.is_dir() {
            return anyhow::anyhow!("not a directory: {}", d.display());
        }
    }
    println!("opening {}...", pretty_tool(tool));
    exec_tool(tool, dir.as_deref())
}

/// c/x/g/a -> the tool a post-switch launch letter means.
fn launch_letter(ans: &str) -> Option<&'static str> {
    match ans.to_ascii_lowercase().as_str() {
        "c" => Some("claude-code"),
        "x" => Some("codex"),
        "g" => Some("gemini"),
        "a" => Some("antigravity"),
        _ => None,
    }
}

/// The persistent full-screen ui: one alternate-screen session, everything
/// inside it. Switch/restore run this same binary as a subprocess (output
/// condensed into the status line - no second switching implementation);
/// opening a conversation is the one action that leaves.
fn ui_tui(paths: &Paths) -> Result<i32> {
    struct Ctx<'a> {
        paths: &'a Paths,
        last_sessions: Vec<MenuSession>,
        /// Timeline emptiness CAPTURED BEFORE the last switch wrote its own
        /// events - the only correct "first time" signal (audit).
        pre_switch_first: bool,
    }
    fn run_self(args: &[&str]) -> (bool, String) {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return (false, format!("cannot find own binary: {e}")),
        };
        match Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                let mut msg = text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("  |  ");
                if msg.chars().count() > 160 {
                    msg = msg.chars().take(159).collect::<String>() + "…";
                }
                (out.status.success(), msg)
            }
            Err(e) => (false, format!("failed: {e}")),
        }
    }
    impl crate::tui::TuiCtx for Ctx<'_> {
        fn rows(&mut self) -> Vec<crate::tui::Row> {
            let Ok(store) = Store::open(self.paths) else {
                return Vec::new();
            };
            let active = active_by_tool(&store, self.paths);
            store
                .list()
                .iter()
                .map(|p| {
                    let (email, tier, marker) = profile_summary(&store, &p.name, &p.tools);
                    let at: Vec<&str> = active
                        .iter()
                        .filter(|(_, n)| n == &p.name)
                        .map(|(t, _)| *t)
                        .collect();
                    crate::tui::Row {
                        name: p.name.clone(),
                        ident: identity_column(email, tier),
                        tools: p
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
                            .join(", "),
                        active: !at.is_empty(),
                        warn: marker,
                    }
                })
                .collect()
        }
        fn switch(&mut self, name: &str) -> (bool, String) {
            self.pre_switch_first = crate::session_link::read_timeline(self.paths).is_empty();
            run_self(&["use", name])
        }
        fn restore(&mut self) -> String {
            run_self(&["restore"]).1
        }
        fn delete(&mut self, name: &str) -> String {
            match Store::open(self.paths).and_then(|s| s.remove(name)) {
                Ok(true) => format!("removed profile '{name}' (the live login stays)"),
                Ok(false) => format!("no profile named '{name}'"),
                Err(e) => format!("delete failed: {e}"),
            }
        }
        fn rename(&mut self, old: &str, new: &str) -> (bool, String) {
            // In-process (like delete) - no subprocess, no current_exe
            // dependency. store.rename also rewrites the timeline internally.
            if !crate::store::valid_profile_name(new) || new == "-" {
                return (false, format!("'{new}' can't be a profile name"));
            }
            let store = match Store::open(self.paths) {
                Ok(s) => s,
                Err(e) => return (false, format!("cannot open store: {e}")),
            };
            let _lock = match store.lock() {
                Ok(g) => g,
                Err(_) => {
                    return (
                        false,
                        "another swapdex is busy (finish or close any open `swapdex login`)".into(),
                    )
                }
            };
            if store.profile_dir_exists(new) {
                return (false, format!("a profile named '{new}' already exists"));
            }
            match store.rename(old, new) {
                Ok(true) => (true, format!("renamed '{old}' -> '{new}'")),
                Ok(false) => (false, format!("no profile named '{old}'")),
                Err(e) => (false, format!("rename failed: {e:#}")),
            }
        }
        fn save_current(&mut self, name: &str) -> (bool, String) {
            // `add <name>` captures the CURRENT live logins (all tools) - no
            // sign-out, no interactive spawn - so it is safe to run in-loop.
            run_self(&["add", name])
        }
        fn doctor(&mut self) -> Vec<String> {
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => return vec![format!("cannot find own binary: {e}")],
            };
            match Command::new(exe)
                .arg("doctor")
                .stdin(std::process::Stdio::null())
                .output()
            {
                Ok(out) => {
                    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    text.lines().map(|l| l.to_string()).collect()
                }
                Err(e) => vec![format!("doctor failed: {e}")],
            }
        }
        fn usage(&mut self) -> Vec<String> {
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => return vec![format!("cannot find own binary: {e}")],
            };
            match Command::new(exe)
                .arg("usage")
                .stdin(std::process::Stdio::null())
                .output()
            {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                    lines.push(String::new());
                    lines.push(
                        "swapdex is local: this is tokens USED here, not remaining quota."
                            .to_string(),
                    );
                    lines
                }
                Err(e) => vec![format!("usage failed: {e}")],
            }
        }
        fn quota(&mut self) -> Vec<String> {
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => return vec![format!("cannot find own binary: {e}")],
            };
            match Command::new(exe)
                .arg("quota")
                .stdin(std::process::Stdio::null())
                .output()
            {
                Ok(out) => {
                    // stderr too (like doctor): a failed quota must show its
                    // error in the panel, not render blank.
                    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    text.lines().map(|l| l.to_string()).collect()
                }
                Err(e) => vec![format!("quota failed: {e}")],
            }
        }
        fn sessionwiki_present(&mut self) -> bool {
            command_exists("sessionwiki")
        }
        fn live_tools(&mut self) -> Vec<String> {
            adapters::all()
                .iter()
                .filter(|a| a.present(self.paths))
                .map(|a| pretty_tool(a.name()).to_string())
                .collect()
        }
        fn sessions(
            &mut self,
            name: &str,
        ) -> (String, Vec<crate::tui::SessionEntry>, Vec<&'static str>) {
            let first_time = self.pre_switch_first;
            let (sessions, label) = recent_menu_sessions(self.paths, name, first_time, 5);
            // The profile's saved tools drive which "open a NEW <tool>" entries
            // the menu offers (a Claude-only account shouldn't offer Codex).
            let tools: Vec<&'static str> = Store::open(self.paths)
                .ok()
                .and_then(|st| st.list().into_iter().find(|p| p.name == name))
                .map(|p| {
                    ["claude-code", "codex", "gemini", "antigravity"]
                        .into_iter()
                        .filter(|t| p.tools.iter().any(|x| x == t))
                        .collect()
                })
                .unwrap_or_default();
            let entries = sessions
                .iter()
                .map(|s| {
                    let (id6, started, tool, title) = s.describe();
                    let age = age_line((started.max(0) as u128) * 1_000_000_000);
                    crate::tui::SessionEntry {
                        line: format!(
                            "{id6}  {:>7}  {}  {}",
                            age,
                            fit(&format!("[{tool}]"), 13),
                            fit(&title, 44)
                        )
                        .trim_end()
                        .to_string(),
                    }
                })
                .collect();
            self.last_sessions = sessions;
            let label = if label.is_empty() {
                format!("open a conversation on '{name}'")
            } else {
                label.trim_end_matches(':').to_string()
            };
            (label, entries, tools)
        }
    }

    let mut ctx = Ctx {
        paths,
        last_sessions: Vec::new(),
        pre_switch_first: crate::session_link::read_timeline(paths).is_empty(),
    };
    loop {
        // An empty store is fine now: the TUI draws an onboarding welcome
        // (offers to save the accounts you're already logged into). Only fall
        // back to text if there is truly nothing to do AND nothing to save.
        if Store::open(paths)?.list().is_empty()
            && adapters::all().iter().all(|a| !a.present(paths))
        {
            println!("No accounts saved yet, and you're not logged into any tool.");
            println!(
                "  sign in to Claude Code / Codex / Gemini / Antigravity, then run `swapdex`."
            );
            return Ok(0);
        }
        match crate::tui::run(&mut ctx)? {
            crate::tui::Outcome::Quit => return Ok(0),
            crate::tui::Outcome::OpenSession(i) => {
                let Some(sess) = ctx.last_sessions.get(i) else {
                    return Ok(0);
                };
                return Err(exec_menu_resume(sess));
            }
            crate::tui::Outcome::NewConv { tool, dir } => {
                println!("opening {}...", pretty_tool(tool));
                return Err(exec_tool(tool, dir.as_deref()));
            }
            crate::tui::Outcome::AddAccount(tool) => {
                let sel = match tool {
                    "claude-code" => Some(ToolSel::Claude),
                    "codex" => Some(ToolSel::Codex),
                    "gemini" => Some(ToolSel::Gemini),
                    _ => Some(ToolSel::Antigravity),
                };
                let who = adapters::by_name(tool)
                    .and_then(|a| a.identity(paths).ok().flatten())
                    .and_then(|id| id.email)
                    .unwrap_or_else(|| "account".into());
                let store = Store::open(paths)?;
                let Some(name) = ask_name(
                    &store,
                    &format!("name for the new account [{}]: ", suggest_name(&who)),
                    &suggest_name(&who),
                ) else {
                    continue;
                };
                drop(store);
                let rc = login(paths, &name, sel)?;
                if rc != 0 {
                    return Ok(rc);
                }
                println!("(press Enter to go back to the picker)");
                let _ = prompt("", "");
            }
        }
    }
}

/// Replace this process with the official tool - the "switch and land in a
/// conversation" handoff. Only returns on exec failure.
fn exec_tool(tool: &str, dir: Option<&std::path::Path>) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let bin = match tool {
        "claude-code" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "antigravity" => "agy",
        other => return anyhow::anyhow!("unknown tool '{other}'"),
    };
    let mut cmd = Command::new(bin);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let err = cmd.exec();
    anyhow::anyhow!("could not launch `{bin}`: {err}")
}

/// Replace this process with `sessionwiki resume <id>` - a one-shot handoff to
/// the official reopen flow (sessionwiki launches the session's own tool).
/// exec(2) only returns on failure, so this returns the error to propagate.
fn exec_sessionwiki_resume(id: &str) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let err = Command::new("sessionwiki")
        .args(["resume", "--no-sync", "--", id])
        .exec();
    anyhow::anyhow!("could not launch `sessionwiki resume {id}`: {err}")
}

/// Turn the macOS Keychain reality into a doctor verdict. `None` = nothing to
/// report (no Claude item found: not logged in, or a locked/headless keychain).
/// `computed` is the item swapdex's own env derives (bare when no env) - the
/// one a `claude` launched from this same shell would read.
///
/// The contract: swapdex manages the profile of the environment it runs in.
/// Other Claude items are OTHER profiles (CLAUDE_CONFIG_DIR aliases) or
/// leftovers - swapdex never touches them, and this verdict says so.
fn keychain_verdict(
    found: &[String],
    target: Option<&str>,
    computed: &str,
) -> Option<(bool, String)> {
    if found.is_empty() {
        return None;
    }
    let list = found
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let Some(t) = target else {
        // The derived item does not exist and several items are present:
        // refusing to guess is the safe behavior, and this is the remedy.
        return Some((
            false,
            format!(
                "this environment's profile item ('{computed}') does not exist; the items \
                 present ({list}) belong to other CLAUDE_CONFIG_DIR profiles, and swapdex \
                 refuses to guess between them. Run swapdex with the profile's \
                 CLAUDE_CONFIG_DIR, or log in once with plain `claude` to create '{computed}'."
            ),
        ));
    };
    if !found.iter().any(|s| s == t) {
        // Defensive: the two keychain reads disagreed (item vanished between).
        return Some((
            false,
            format!(
                "swapdex resolves '{t}' but the Keychain currently shows {list} - re-run \
                 `swapdex doctor`; if this persists, launch swapdex with the same \
                 CLAUDE_CONFIG_DIR you launch `claude` with."
            ),
        ));
    }
    if t != computed {
        // Single-item fallback: swapdex's env derives a missing item, so it
        // manages the only login that exists. Working alias-only setup.
        return Some((
            true,
            format!(
                "this environment derives '{computed}' (not present); managing the only \
                 Claude login, '{t}' - if your `claude` runs with a CLAUDE_CONFIG_DIR, \
                 launch swapdex with the same one"
            ),
        ));
    }
    let msg = if found.len() > 1 {
        format!(
            "managing this environment's profile ('{t}'); {} other Claude item(s) belong to \
             other CLAUDE_CONFIG_DIR profiles (or are leftovers) - swapdex never touches them",
            found.len() - 1
        )
    } else {
        format!("managing this environment's profile ('{t}')")
    };
    Some((true, msg))
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

    // macOS: swapdex swaps Claude's login INSIDE the Keychain, so a mismatch
    // between the item Claude reads and the one swapdex writes is the classic
    // "I switched but the old account is still active". Read-only; no-op off
    // macOS (Claude is file-based there).
    if let Some(diag) = crate::adapters::claude::keychain_diagnostic() {
        if let Some((ok, msg)) =
            keychain_verdict(&diag.found, diag.target.as_deref(), &diag.computed)
        {
            report("keychain", ok, msg);
        }
        if let Some(dir) = &diag.config_dir {
            report(
                "config-dir",
                true,
                format!(
                    "CLAUDE_CONFIG_DIR={} (swapdex must see the same value)",
                    crate::util::redact_path(dir)
                ),
            );
        }
    }

    // Live credential files hold refresh tokens - flag loose modes on ALL of
    // them, not just .claude.json (the store already self-tightens; the live
    // files are each tool's own, so we can only warn).
    for f in [
        paths.claude_credentials(),
        paths.codex_auth(),
        paths.gemini_oauth(),
        paths.antigravity_token(),
    ] {
        if let Ok(meta) = std::fs::metadata(&f) {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                report(
                    "perms",
                    false,
                    format!(
                        "{} is group/world-readable (holds tokens); run `chmod 600` on it",
                        crate::util::redact_path(&f.display().to_string())
                    ),
                );
            }
        }
    }
    // A corrupt live .claude.json breaks every claude switch with an error
    // users misread as a snapshot problem - diagnose it here by name.
    if paths.claude_config_json().exists() {
        if let Ok(bytes) = std::fs::read(paths.claude_config_json()) {
            if serde_json::from_slice::<Value>(&bytes).is_err() {
                report(
                    "claude-config",
                    false,
                    format!(
                        "{} is not valid JSON - claude switches will fail until it is \
                         repaired or removed (removing loses local settings like \
                         project trust)",
                        crate::util::redact_path(&paths.claude_config_json().display().to_string())
                    ),
                );
            }
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
    for tool in ["claude-code", "codex", "gemini", "antigravity"] {
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
    for cli in ["claude", "codex", "gemini", "agy"] {
        if command_exists(cli) {
            found.push(cli);
        }
    }
    report(
        "tools",
        true,
        if found.is_empty() {
            "none of `claude`, `codex`, `gemini`, `agy` found on PATH".into()
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
        Err(crate::store::LockError::Busy) => {
            eprintln!(
                "swapdex: another swapdex is busy (a switch, or a `swapdex login` waiting \
                 for a sign-in). Finish or close it, then retry."
            );
            return Ok(4);
        }
        Err(crate::store::LockError::Unwritable(e)) => {
            eprintln!(
                "swapdex: the store is not writable ({e}) - check permissions/mount of \
                 the store directory"
            );
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
        Err(crate::store::LockError::Busy) => {
            eprintln!(
                "swapdex: another swapdex is busy (a switch, or a `swapdex login` waiting \
                 for a sign-in). Finish or close it, then retry."
            );
            return Ok(4);
        }
        Err(crate::store::LockError::Unwritable(e)) => {
            eprintln!(
                "swapdex: the store is not writable ({e}) - check permissions/mount of \
                 the store directory"
            );
            return Ok(4);
        }
    };
    // Source must be a REAL profile (ghost dirs with no known tools are
    // hidden from ls - acting on them here would contradict it)...
    if !store.list().iter().any(|p| p.name == old) {
        eprintln!("swapdex: no profile named '{old}'");
        return Ok(5);
    }
    // ...while the TARGET collision checks the directory itself: colliding
    // with a hidden ghost dir must still be a clean "exists" (6), not a
    // hard error from the rename syscall.
    if store.profile_dir_exists(new) {
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
    if let Some(c) = reject_bad_name(name).or_else(|| reject_reserved_name(name)) {
        return Ok(c);
    }
    let tool = match sel {
        Some(ToolSel::Claude) => "claude-code",
        Some(ToolSel::Codex) => "codex",
        Some(ToolSel::Gemini) => "gemini",
        Some(ToolSel::Antigravity) => "antigravity",
        _ => {
            // Never guess which tool the user means (real-use feedback: the
            // old codex-if-installed default kept asking about the wrong
            // tool). On a terminal, ask; otherwise require --tool.
            use std::io::IsTerminal;
            let tty =
                std::io::stdin().is_terminal() || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
            if !tty {
                eprintln!("swapdex: say which tool: swapdex login {name} --tool <claude|codex|gemini|antigravity>");
                return Ok(2);
            }
            println!("Which tool do you want to log '{name}' into?");
            println!("  1) Claude Code   2) Codex   3) Gemini CLI   4) Antigravity");
            loop {
                match prompt("pick [1-4] (Enter cancels): ", "").as_deref() {
                    Some("1") => break "claude-code",
                    Some("2") => break "codex",
                    Some("3") => break "gemini",
                    Some("4") => break "antigravity",
                    Some("") | None => {
                        println!("cancelled.");
                        return Ok(0);
                    }
                    _ => println!("pick a number between 1 and 4 (Enter cancels)"),
                }
            }
        }
    };

    // One flow for all four tools from here on.
    let bin = match tool {
        "claude-code" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        _ => "agy",
    };
    if !command_exists(bin) {
        // stderr + exit 3: `login x && ...` in a script must not proceed as
        // if a login was saved.
        eprintln!("swapdex: `{bin}` isn't on your PATH. Install it, then retry.");
        return Ok(3);
    }
    let adapter = adapters::by_name(tool).expect("known tool");
    let flag = pretty_tool_flag(tool);

    let Some(cur) = adapter.identity(paths).ok().flatten() else {
        // Not logged in at all: run the tool's own sign-in, then capture.
        // (codex has a real `login` subcommand; the others sign in on first
        // run of the app itself.)
        println!(
            "Opening {} to sign in. Complete the login{}",
            pretty_tool(tool),
            if tool == "codex" {
                " in your browser.".to_string()
            } else {
                ", then exit it.".to_string()
            }
        );
        spawn_tool_login(bin, tool)?;
        if adapter.identity(paths).ok().flatten().is_none() {
            eprintln!(
                "swapdex: no {} login was completed - nothing saved.",
                pretty_tool(tool)
            );
            return Ok(8);
        }
        println!();
        // update=true so re-running `login <name>` refreshes an existing profile.
        return add(paths, Some(name), sel_for_tool(tool), true);
    };

    // Already logged in - the user wants to ADD a different account. Do the
    // whole thing: save the current login, sign out locally, run the tool's
    // sign-in, capture the new account. The original login is stashed in the
    // store and restored on any failure, so this can never lose an account.
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal() || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
    if !tty {
        // Scripts get guidance - this flow is interactive.
        println!(
            "You're already logged into {} ({}).",
            pretty_tool(tool),
            identity_line(&cur)
        );
        println!("  save the current account:  swapdex add {name} --tool {flag}");
        println!(
            "  add a DIFFERENT account:   swapdex login {name} --tool {flag}  (on a terminal)"
        );
        // Exit 3, not 0: `login x && use x` in a script must not proceed as
        // if a login was saved (nothing was).
        return Ok(3);
    }
    println!("Currently logged in as {}.", identity_line(&cur));
    if !yes_no(
        &format!(
            "Sign in to a DIFFERENT account as '{name}'? swapdex will save the \
             current login, sign you out locally, and open {} for the \
             new sign-in. [Y/n]: ",
            pretty_tool(tool)
        ),
        true,
    ) {
        println!("cancelled - nothing changed.");
        return Ok(0);
    }
    let store = Store::open(paths)?;
    let lock1 = match store.lock() {
        Ok(g) => g,
        Err(crate::store::LockError::Busy) => {
            eprintln!(
                "swapdex: another swapdex is busy (a switch, or a `swapdex login` waiting \
                 for a sign-in). Finish or close it, then retry."
            );
            return Ok(4);
        }
        Err(crate::store::LockError::Unwritable(e)) => {
            eprintln!(
                "swapdex: the store is not writable ({e}) - check permissions/mount of \
                 the store directory"
            );
            return Ok(4);
        }
    };
    // 1) The current login, saved twice over: a store backup (restore can
    //    always bring it back) plus a refresh of every profile holding it -
    //    and, if unmatched, an offer to keep it under a name.
    let stash = adapter.capture(paths)?;
    store.backup(&stash)?;
    for pname in matching_profile_names(&store, tool, &cur.account_id) {
        store.save(&pname, &stash)?;
    }
    if matched_profile_name(&store, tool, &cur.account_id).is_none() {
        // No email on disk (antigravity): suggest a plain name instead of a
        // sanitized display string like "GoogleaccountAntigravity".
        let suggestion = match &cur.email {
            Some(e) => suggest_name(e),
            None => "main".to_string(),
        };
        while let Some(keep) = ask_name(
            &store,
            &format!("name to keep the CURRENT account under [{suggestion}]: "),
            &suggestion,
        ) {
            if keep == name {
                // '{name}' is reserved for the NEW account - accepting it here
                // would let the new sign-in silently overwrite the current one.
                println!("'{name}' is the name for the NEW account - pick another.");
                continue;
            }
            store.save(&keep, &stash)?;
            println!("saved current login as '{keep}'.");
            break;
        }
    }
    // 2) Local sign-out, so the tool's own flow prompts a FRESH sign-in.
    sign_out_locally(paths, tool);
    // Verify the sign-out actually took. Two independent checks:
    // - identity: same account still resolvable (e.g. an aliased
    //   CLAUDE_CONFIG_DIR kept .claude.json's oauthAccount alive);
    // - present: a CREDENTIAL still lingers even with the identity gone
    //   (e.g. a second suffixed macOS Keychain item that keychain_delete's
    //   discovery did not target). Proceeding then is worse than the trust
    //   prompt: the eventual capture could pair the OLD token with the NEW
    //   account's identity - a profile that switches to the wrong login.
    // Either way: abort clearly and restore.
    let still_same = adapter
        .identity(paths)
        .ok()
        .flatten()
        .is_some_and(|still| still.account_id == cur.account_id);
    if still_same || adapter.present(paths) {
        adapter.apply(paths, &stash)?;
        drop(lock1);
        eprintln!(
            "swapdex: couldn't sign {} out of the current account ({}), so a new \
             account can't be added this way - your login is unchanged.",
            pretty_tool(tool),
            identity_line(&cur)
        );
        eprintln!("  {}", same_account_hint(tool));
        return Ok(0);
    }
    // RELEASE the store lock before the interactive sign-in: it can take
    // minutes (or be left open), and holding it would block every other
    // swapdex - rename, use, everything - with "another swapdex is
    // mid-switch". The stash is already safe in the store's backups.
    drop(lock1);
    // 3) Fresh sign-in inside the official app.
    println!(
        "Opening {} - sign in with the OTHER account{}",
        pretty_tool(tool),
        if tool == "codex" {
            " in your browser.".to_string()
        } else {
            ", then exit it.".to_string()
        }
    );
    // Proactive warning: the tool may re-use a cached browser session and log
    // you straight back into the SAME account without asking. Tell them how to
    // avoid it BEFORE it happens.
    println!("  tip: {}", same_account_hint(tool));
    let spawn = spawn_tool_login(bin, tool);
    // Re-take the lock for the final store writes. Best-effort: the sign-in is
    // done, a single profile save is an atomic per-file write, and we must not
    // discard the user's completed login just because another swapdex is
    // momentarily active.
    let _lock = store.lock().ok();
    // 4) Capture, or restore the stash on any failure.
    let new_id = adapter.identity(paths).ok().flatten();
    match (spawn, new_id) {
        (Ok(status), Some(new)) if !new.account_id.is_empty() => {
            if new.account_id == cur.account_id {
                // The tool re-used its browser session and signed BACK INTO
                // the same account - swapdex removed the local login, but it
                // cannot force the tool's OAuth to offer an account picker.
                // Do NOT save a duplicate profile under a new name; explain
                // how to actually reach the other account, and restore the
                // stash so the login is exactly as it was.
                adapter.apply(paths, &stash)?;
                eprintln!(
                    "swapdex: you were signed back into the SAME account ({}), so \
                     nothing was saved as '{name}'.",
                    identity_line(&new)
                );
                eprintln!("  {}", same_account_hint(tool));
                eprintln!(
                    "  (to just save THIS account under a name, use `swapdex add {name} \
                     --tool {}`.)",
                    pretty_tool_flag(tool)
                );
                return Ok(0);
            }
            // Same repoint rule as `add --update`: if '{name}' already has a
            // snapshot for this tool, changing what the name means must be
            // explicit. An UNREADABLE snapshot counts as "different" - corrupt
            // and absent must not be conflated, or the guard is bypassable.
            let has_tool_snapshot = store
                .list()
                .iter()
                .any(|p| p.name == name && p.tools.iter().any(|t| t == tool));
            let same_account = profile_account_id(&store, name, tool)
                .filter(|s| !s.is_empty())
                .as_deref()
                == Some(new.account_id.as_str());
            if has_tool_snapshot
                && !same_account
                && !yes_no(
                    &format!(
                        "profile '{name}' already holds a different (or unreadable) \
                         {tool} account. Repoint it to this new login? [y/N]: "
                    ),
                    false,
                )
            {
                // The user completed a REAL sign-in - never discard it
                // silently. Offer a different name; skipping restores the
                // stash and honestly says the new sign-in is gone.
                if let Some(rescue) = ask_name(
                    &store,
                    "save the NEW account under a different name instead (Enter discards it): ",
                    "",
                ) {
                    if rescue != name {
                        let snap = adapter.capture(paths)?;
                        store.save(&rescue, &snap)?;
                        println!(
                            "saved profile '{rescue}' ({}). '{name}' is untouched.",
                            identity_line(&new)
                        );
                        println!("switch back any time:  swapdex use <name>  (or `swapdex ui`)");
                        return Ok(0);
                    }
                }
                adapter.apply(paths, &stash)?;
                println!(
                    "the new sign-in was DISCARDED and your previous login restored - \
                     '{name}' is untouched. Re-run `swapdex login <other-name>` to \
                     redo it under another name."
                );
                return Ok(0);
            }
            let snap = adapter.capture(paths)?;
            store.save(name, &snap)?;
            println!("saved profile '{name}' ({}).", identity_line(&new));
            if tool == "antigravity" {
                // Honesty over silence: the token file stores no email or
                // account id, so the same-account check above can never fire
                // here and ls cannot show WHO this is.
                println!(
                    "note: Antigravity stores no account identity on disk - swapdex \
                     cannot confirm WHICH Google account this is; verify inside agy."
                );
            }
            if !status.success() {
                println!(
                    "note: {} exited with an error after signing in - if anything \
                     looks off, `swapdex restore --tool {flag}` undoes this.",
                    pretty_tool(tool)
                );
            }
            println!("switch back any time:  swapdex use <name>  (or `swapdex ui`)");
            Ok(0)
        }
        _ => {
            adapter.apply(paths, &stash)?;
            eprintln!(
                "swapdex: no new {} login was completed - your previous \
                 login ({}) was restored.",
                pretty_tool(tool),
                identity_line(&cur)
            );
            Ok(8)
        }
    }
}

/// The ToolSel a canonical tool name maps back to.
fn sel_for_tool(tool: &str) -> Option<ToolSel> {
    match tool {
        "claude-code" => Some(ToolSel::Claude),
        "codex" => Some(ToolSel::Codex),
        "gemini" => Some(ToolSel::Gemini),
        "antigravity" => Some(ToolSel::Antigravity),
        _ => None,
    }
}

/// Run the tool's own sign-in command, terminal inherited. codex has a real
/// `login` subcommand; the other three sign in on first run of the app.
fn spawn_tool_login(bin: &str, tool: &str) -> Result<std::process::ExitStatus> {
    // A shell Ctrl+C during the interactive sign-in hits the whole foreground
    // process group. With the default disposition it would kill swapdex before
    // the restore-stash branch runs - leaving the user locally signed out of
    // everything. A no-op HANDLER (not SIG_IGN: handlers reset to default
    // across exec, SIG_IGN would be inherited by the child) makes swapdex
    // ride it out; the child keeps normal Ctrl+C behavior.
    unsafe extern "C" fn ride_out(_: libc::c_int) {}
    #[allow(function_casts_as_integer)]
    let prev_int = unsafe { libc::signal(libc::SIGINT, ride_out as libc::sighandler_t) };
    #[allow(function_casts_as_integer)]
    let prev_quit = unsafe { libc::signal(libc::SIGQUIT, ride_out as libc::sighandler_t) };
    let mut cmd = Command::new(bin);
    match tool {
        // Codex has a `login` subcommand; Claude Code a proper `auth login`
        // that does JUST the OAuth sign-in (no workspace-trust / session
        // detour); Gemini / Antigravity sign in on first run of the app.
        "codex" => {
            cmd.arg("login");
        }
        "claude-code" => {
            cmd.args(["auth", "login"]);
        }
        _ => {}
    }
    let status = cmd.status();
    unsafe {
        libc::signal(libc::SIGINT, prev_int);
        libc::signal(libc::SIGQUIT, prev_quit);
    }
    status.map_err(|e| anyhow::anyhow!("could not run {bin}: {e}"))
}

/// Remove the live credential files so the tool's next run prompts a fresh
/// sign-in. Claude keeps the rest of .claude.json (projects, settings) - only
/// the oauthAccount block goes.
fn sign_out_locally(paths: &Paths, tool: &str) {
    match tool {
        "claude-code" => {
            // Sign out LOCALLY only - deliberately NOT `claude auth logout`.
            // That command REVOKES the OAuth token server-side, which kills the
            // snapshot we captured one step earlier AND every saved profile
            // that shares this account - the "all my logins got signed out"
            // disaster. A safe switcher must never destroy a login it exists to
            // preserve. Clearing the local Keychain item + credential file is
            // enough for Claude to prompt a fresh sign-in for the NEW account,
            // and the stashed token stays valid so `restore` / switching back
            // still works. This matches what claude-swap and Symbioose do
            // (local `security delete`, never a server-revoking logout).
            std::fs::remove_file(paths.claude_credentials()).ok();
            crate::adapters::claude::keychain_delete();
            if let Ok(bytes) = std::fs::read(paths.claude_config_json()) {
                if let Ok(mut cfg) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(obj) = cfg.as_object_mut() {
                        obj.remove("oauthAccount");
                        if let Ok(out) = serde_json::to_vec(&cfg) {
                            let _ = crate::atomic::write_secret(&paths.claude_config_json(), &out);
                        }
                    }
                }
            }
        }
        "codex" => {
            std::fs::remove_file(paths.codex_auth()).ok();
        }
        "gemini" => {
            std::fs::remove_file(paths.gemini_oauth()).ok();
            std::fs::remove_file(paths.gemini_accounts()).ok();
        }
        _ => {
            std::fs::remove_file(paths.antigravity_token()).ok();
        }
    }
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
/// How to actually reach a DIFFERENT account when the tool keeps signing you
/// back into the same one from a cached browser session. swapdex removes the
/// local credential but cannot control the tool's own OAuth prompt.
fn same_account_hint(tool: &str) -> String {
    match tool {
        "claude-code" => "To add a different account: sign out at claude.ai in your browser \
             first (or use /logout then /login inside Claude Code and pick the other \
             account), then run this again."
            .to_string(),
        "codex" => "Codex re-used your ChatGPT browser session. Sign out at chatgpt.com \
             (or open the login in a different browser / private window), then run this \
             again."
            .to_string(),
        _ => "The tool re-used your signed-in Google account. Choose the OTHER account at \
             Google's account picker (or sign the first one out in your browser), then \
             run this again."
            .to_string(),
    }
}

fn pretty_tool(tool: &str) -> &str {
    match tool {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "gemini" => "Gemini CLI",
        "antigravity" => "Antigravity",
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
            println!("  '{ans}' can't be a name (1-64 bytes, not all spaces; no '/', '\\\\', leading '.', or control chars). Try again.");
            continue;
        }
        // ask_name only ever names a NEW profile, so reject the reserved "-"
        // here too - `valid_profile_name` intentionally allows it (legacy "-"
        // profiles must stay rm/rename-able), but CREATION must not, or setup
        // would mint a "-" that breaks `use -`. `add`/`rename` reject it via a
        // separate post-check; setup had none.
        if ans == "-" {
            println!(
                "  '-' is reserved (`swapdex use -` toggles to the previous profile). Try again."
            );
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
    println!(
        "swapdex keeps several Claude Code / Codex / Gemini / Antigravity logins and switches between them."
    );
    println!(
        "Let's save the accounts you use. Press Enter to accept a [default], Ctrl-C to quit.\n"
    );

    // 1) Save the accounts you're currently logged into.
    for adapter in adapters::all() {
        let tool = adapter.name();
        // A corrupt/unreadable login for ONE tool (e.g. a hand-edited
        // ~/.claude.json) must not abort the whole wizard before the other,
        // valid tools get saved. Treat an error like "not logged in": warn
        // and continue to the next tool.
        let id = match adapter.identity(paths) {
            Ok(Some(id)) => id,
            Ok(None) => {
                println!("{}: not logged in - skipping.\n", pretty_tool(tool));
                continue;
            }
            Err(e) => {
                println!(
                    "{}: login present but unreadable ({}) - skipping.\n",
                    pretty_tool(tool),
                    crate::util::redact_path(&format!("{e:#}"))
                );
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
        // Attaching this tool to an existing profile of the same suggested
        // name is the NORMAL multi-tool case (`swapdex add <name>` semantics)
        // - never scare with "replace it?" for it, and never skip it.
        if let Some(p) = store.list().into_iter().find(|p| p.name == default) {
            if !p.tools.iter().any(|t| t == tool) {
                // One unreadable tool must not abort the whole wizard.
                match adapter.capture(paths) {
                    Ok(snap) => {
                        store.save(&default, &snap)?;
                        println!("  attached {} to '{default}'.\n", pretty_tool(tool));
                    }
                    Err(e) => {
                        eprintln!("  could not read this login ({e:#}) - skipped.\n");
                    }
                }
                continue;
            }
        }
        match ask_name(
            &store,
            &format!("  save it as [{default}] (Enter to accept, 'skip' to skip): "),
            &default,
        ) {
            Some(name) => match adapter.capture(paths) {
                Ok(snap) => {
                    store.save(&name, &snap)?;
                    println!("  saved as '{name}'.\n");
                }
                Err(e) => {
                    eprintln!("  could not read this login ({e:#}) - skipped.\n");
                }
            },
            None => println!("  skipped.\n"),
        }
    }

    // 2) Offer to add more accounts - ANY tool, through the same one-flow
    //    login (save current, sign out locally, fresh sign-in, capture).
    println!("You can keep several accounts per tool (e.g. work and personal).");
    loop {
        if !yes_no("  add another account now? [y/N]: ", false) {
            break;
        }
        println!("  which tool?  1) Claude Code   2) Codex   3) Gemini CLI   4) Antigravity");
        let sel = loop {
            match prompt("  pick [1-4] (Enter cancels): ", "").as_deref() {
                Some("1") => break Some(ToolSel::Claude),
                Some("2") => break Some(ToolSel::Codex),
                Some("3") => break Some(ToolSel::Gemini),
                Some("4") => break Some(ToolSel::Antigravity),
                Some("") | None => break None,
                _ => println!("  pick a number between 1 and 4 (Enter cancels)"),
            }
        };
        let Some(sel) = sel else {
            println!("  skipped.\n");
            continue;
        };
        let name = match ask_name(&store, "  name for it (e.g. personal): ", "") {
            Some(n) => n,
            None => {
                println!("  skipped.\n");
                continue;
            }
        };
        let _ = login(paths, &name, Some(sel))?;
        println!();
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
                let accounts: serde_json::Map<String, Value> = r
                    .accounts
                    .iter()
                    .map(|(name, (t5, t7))| {
                        (
                            name.clone(),
                            serde_json::json!({"last_5h_tokens": t5, "last_7d_tokens": t7}),
                        )
                    })
                    .collect();
                serde_json::json!({
                    "tool": r.tool,
                    "last_5h": {"sessions": r.w5h.sessions, "tokens": r.w5h.tokens},
                    "last_7d": {"sessions": r.w7d.sessions, "tokens": r.w7d.tokens},
                    "accounts": accounts,
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
        // Per-account rows via the switch timeline; the untagged remainder is
        // whatever predates the first switch.
        for (name, (t5, t7)) in &r.accounts {
            println!(
                "    @{:<11} 5h: {:>7} tok           7d: {:>8} tok",
                name,
                crate::usage::human(*t5),
                crate::usage::human(*t7),
            );
        }
        let attributed7: u64 = r.accounts.values().map(|(_, t7)| *t7).sum();
        let rest = r.w7d.tokens.saturating_sub(attributed7);
        if !r.accounts.is_empty() && rest > 0 {
            println!(
                "    {:<12} 5h:                       7d: {:>8} tok (before your first switch)",
                "(untagged)",
                crate::usage::human(rest),
            );
        }
    }
    // Honesty for the two tools usage CANNOT cover: they keep no token
    // transcripts on disk, so a gemini/antigravity-heavy user must not read
    // the silence as "no usage".
    let uncovered: Vec<&str> = ["gemini", "antigravity"]
        .into_iter()
        .filter(|t| {
            adapters::by_name(t)
                .map(|a| a.present(paths))
                .unwrap_or(false)
        })
        .collect();
    if !uncovered.is_empty() {
        println!(
            "note: {} not shown - those CLIs keep no local token transcripts to read",
            uncovered.join(" and ")
        );
    }
    println!("(summed locally from session transcripts; accounts via the switch timeline)");
    Ok(0)
}

/// `swapdex quota` - the one opt-in network command. Reads each Claude account's
/// REMAINING quota from Anthropic's usage endpoint (that account's own token,
/// read-only, zero message spend). The active account uses its live token; a
/// saved-but-inactive account uses its snapshot token, which may have expired
/// (swapdex does not refresh tokens - that is the switcher/rotator line). All
/// network rules live in src/quota.rs; this function only orchestrates + renders.
pub fn quota(paths: &Paths, json: bool) -> Result<i32> {
    use crate::quota::{self as q, Fetch};

    struct Row {
        /// Display label (may carry an "(active)" marker).
        label: String,
        /// The plain profile name - what `--json` reports and `use` hints take.
        name: String,
        email: Option<String>,
        token: Option<String>,
        active: bool,
    }

    let live_id = adapters::claude::Claude.identity(paths).ok().flatten();
    let live_uuid = live_id
        .as_ref()
        .map(|a| a.account_id.clone())
        .filter(|s| !s.is_empty());
    let live_token = adapters::claude::live_credentials(paths)
        .as_deref()
        .and_then(q::token_from_credentials);

    let mut rows: Vec<Row> = Vec::new();
    let mut matched_live = false;
    if let Ok(store) = Store::open(paths) {
        for p in store.list() {
            if !p.tools.iter().any(|t| t == "claude-code") {
                continue;
            }
            let snap = store.load(&p.name, "claude-code").ok().flatten();
            let (mut email, mut uuid, mut token) = (None, None, None);
            if let Some(s) = &snap {
                if let Some(o) = s
                    .part("oauth_account")
                    .and_then(|o| serde_json::from_slice::<Value>(o.expose()).ok())
                {
                    email = o["emailAddress"].as_str().map(str::to_string);
                    uuid = o["accountUuid"].as_str().map(str::to_string);
                }
                token = s
                    .part("credentials")
                    .and_then(|c| q::token_from_credentials(c.expose()));
            }
            let active = live_uuid.is_some() && uuid == live_uuid;
            matched_live |= active;
            rows.push(Row {
                label: if active {
                    format!("{} (active)", p.name)
                } else {
                    p.name.clone()
                },
                name: p.name.clone(),
                email: if active {
                    live_id.as_ref().and_then(|a| a.email.clone()).or(email)
                } else {
                    email
                },
                token: if active { live_token.clone() } else { token },
                active,
            });
        }
    }
    // A live login that is not saved as any profile still deserves a line.
    if !matched_live && live_token.is_some() {
        rows.insert(
            0,
            Row {
                label: "(active login, not saved)".into(),
                name: "(active login, not saved)".into(),
                email: live_id.as_ref().and_then(|a| a.email.clone()),
                token: live_token.clone(),
                active: true,
            },
        );
    }
    rows.sort_by_key(|r| !r.active);

    if rows.is_empty() {
        if json {
            println!("{}", serde_json::json!({"accounts": [], "offline": null}));
        } else {
            println!(
                "No Claude accounts found. Log in with `claude`, or `swapdex add` to save one."
            );
        }
        return Ok(0);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Fetch each account. If the first attempt that actually leaves the machine
    // fails at the transport layer, we are almost certainly offline - stop
    // rather than fire every account's token at an unreachable endpoint.
    let mut results: Vec<(usize, Fetch)> = Vec::new();
    let mut offline: Option<String> = None;
    for (i, r) in rows.iter().enumerate() {
        match &r.token {
            None => results.push((i, Fetch::Offline("no saved token".into()))),
            // An unusable token is a PER-ACCOUNT problem (corrupt snapshot),
            // not a transport failure - it must never masquerade as "the
            // network is down" and abort the whole run.
            Some(t) if !q::token_usable(t) => results.push((
                i,
                Fetch::Offline(
                    "saved token unusable (corrupt snapshot?) - `swapdex add <name> --update` \
                     re-saves it"
                        .into(),
                ),
            )),
            Some(t) => {
                let f = q::fetch(t);
                let reached = results.iter().any(|(_, x)| !matches!(x, Fetch::Offline(_)));
                if !reached {
                    if let Fetch::Offline(msg) = &f {
                        offline = Some(msg.clone());
                        break;
                    }
                }
                results.push((i, f));
            }
        }
    }

    if json {
        let accounts: Vec<Value> = results
            .iter()
            .map(|(i, f)| {
                quota_json(
                    &rows[*i].name,
                    rows[*i].email.as_deref(),
                    rows[*i].active,
                    f,
                )
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"accounts": accounts, "offline": offline})
        );
        return Ok(0);
    }

    if let Some(msg) = offline {
        println!("swapdex quota: could not reach api.anthropic.com - {msg}");
        println!(
            "(quota is the only swapdex command that uses the network; everything else is local)"
        );
        return Ok(0);
    }

    println!("quota - remaining on your Claude accounts");
    println!("live from Anthropic's usage endpoint; opt-in network, spends 0 message quota.\n");
    for (i, f) in &results {
        let r = &rows[*i];
        match &r.email {
            Some(e) => println!("{}   {}", r.label, e),
            None => println!("{}", r.label),
        }
        match f {
            Fetch::Ok(qd) => {
                let mut any = false;
                if let Some(w) = qd.five_hour {
                    println!("  {}", win_line("5h", &w, now));
                    any = true;
                }
                if let Some(w) = qd.seven_day {
                    println!("  {}", win_line("7d", &w, now));
                    any = true;
                }
                for (label, w) in &qd.scoped {
                    println!("  {}", win_line(label, w, now));
                    any = true;
                }
                if !any {
                    println!(
                        "  (endpoint reported no windows - `swapdex quota --json` to inspect)"
                    );
                }
            }
            Fetch::Unauthorized => {
                if r.active {
                    println!("  active token rejected - run `claude` once to refresh, then retry");
                } else {
                    println!(
                        "  snapshot token expired - `swapdex use {}` to refresh, then `swapdex quota`",
                        r.name
                    );
                }
            }
            Fetch::Unexpected(code, _) => {
                println!(
                    "  unexpected response (HTTP {code}) - run `swapdex quota --json` to see it"
                );
            }
            Fetch::Offline(msg) => println!("  {msg}"),
        }
        println!();
    }
    println!("this is the only swapdex command that touches the network.");
    Ok(0)
}

/// Render one window as a remaining-percent bar with its reset countdown.
fn win_line(label: &str, w: &crate::quota::Window, now: i64) -> String {
    let rem = w.remaining_pct();
    let filled = ((rem / 100.0) * 10.0).round().clamp(0.0, 10.0) as usize;
    let bar: String = "\u{2593}".repeat(filled) + &"\u{2591}".repeat(10 - filled);
    let reset = match w.resets_at {
        Some(ts) => format!("   resets in {}", human_until(now, ts)),
        None => String::new(),
    };
    format!("{label:<9} {bar}  {rem:>3.0}% left{reset}")
}

/// A coarse "2h 14m" / "3d 4h" countdown to `ts` from `now` (unix seconds).
fn human_until(now: i64, ts: i64) -> String {
    let d = ts - now;
    if d <= 0 {
        return "now".into();
    }
    let (days, hrs, mins) = (d / 86400, (d % 86400) / 3600, (d % 3600) / 60);
    if days > 0 {
        format!("{days}d {hrs}h")
    } else if hrs > 0 {
        format!("{hrs}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

/// One account's quota as JSON (for `swapdex quota --json`). An unexpected shape
/// carries the raw body so the exact endpoint schema is never lost.
fn quota_json(label: &str, email: Option<&str>, active: bool, f: &crate::quota::Fetch) -> Value {
    use crate::quota::{Fetch, Window};
    fn win(w: &Window) -> Value {
        serde_json::json!({
            "used_pct": (w.used_pct * 10.0).round() / 10.0,
            "remaining_pct": (w.remaining_pct() * 10.0).round() / 10.0,
            "resets_at": w.resets_at,
        })
    }
    let mut o = serde_json::json!({"name": label, "email": email, "active": active});
    let m = o.as_object_mut().expect("json object");
    match f {
        Fetch::Ok(q) => {
            m.insert("status".into(), Value::String("ok".into()));
            m.insert(
                "five_hour".into(),
                q.five_hour.as_ref().map(win).unwrap_or(Value::Null),
            );
            m.insert(
                "seven_day".into(),
                q.seven_day.as_ref().map(win).unwrap_or(Value::Null),
            );
            let scoped: Vec<Value> = q
                .scoped
                .iter()
                .map(|(n, w)| {
                    let mut wj = win(w);
                    wj.as_object_mut()
                        .unwrap()
                        .insert("label".into(), Value::String(n.clone()));
                    wj
                })
                .collect();
            m.insert("scoped".into(), Value::Array(scoped));
        }
        Fetch::Unauthorized => {
            m.insert("status".into(), Value::String("expired".into()));
        }
        Fetch::Unexpected(code, body) => {
            m.insert("status".into(), Value::String("unexpected".into()));
            m.insert("http".into(), Value::from(*code));
            m.insert("raw".into(), Value::String(body.clone()));
        }
        Fetch::Offline(msg) => {
            m.insert("status".into(), Value::String("offline".into()));
            m.insert("detail".into(), Value::String(msg.clone()));
        }
    }
    o
}

pub fn sessions(paths: &Paths, json: bool) -> Result<i32> {
    if json {
        // Scripting parity with the human view: {"accounts": {...}, "total": N}.
        // available=false distinguishes "no sessionwiki" from "zero sessions".
        let out = match crate::session_link::sessions_by_account(paths) {
            None => serde_json::json!({"available": false, "accounts": {}, "total": 0}),
            Some(counts) => {
                let total: usize = counts.values().sum();
                serde_json::json!({"available": true, "accounts": counts, "total": total})
            }
        };
        println!("{}", serde_json::to_string(&out)?);
        return Ok(0);
    }
    match crate::session_link::sessions_by_account(paths) {
        None => {
            println!(
                "session data unavailable - install sessionwiki to group sessions by account \
                 (`swapdex ui` already lists your recent sessions without it)"
            );
        }
        Some(counts) if counts.is_empty() => {
            // sessionwiki responded but its index is empty - the fresh-install
            // landmine. Say the one command that fixes it.
            println!("no sessions found (sessionwiki index empty - run `sessionwiki sync` once)");
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
    // expiresAt is epoch millis. An OAuth ACCESS token lapses about hourly and
    // the tool refreshes it silently, so "expired" for a just-lapsed token is
    // pure noise (this is the `status` twin of the 0.20.0 ls/marker fix that
    // this line was missed by). Only note a snapshot older than STALE_DAYS,
    // where the refresh token itself may be dead and a re-login is plausible.
    match expires_at {
        Some(ms) if now_ms() - ms > STALE_DAYS * 86400 * 1000 => {
            " - login is old; may re-prompt if its refresh token has expired".to_string()
        }
        _ => String::new(),
    }
}

fn warn_if_expired(target: &crate::adapters::Snapshot, tool: &str) {
    if tool != "claude-code" {
        return;
    }
    if let Some(cred) = target.part("credentials") {
        if let Ok(v) = serde_json::from_slice::<Value>(cred.expose()) {
            // Only warn for an ANCIENT snapshot (>30d) whose refresh token may
            // be dead - a normally-expired access token (~1h) is refreshed
            // silently, so warning every switch was noise.
            if let Some(ms) = v["claudeAiOauth"]["expiresAt"].as_i64() {
                if now_ms() - ms > STALE_DAYS * 86400 * 1000 {
                    eprintln!("swapdex: note - this saved login is old; Claude may re-prompt for login if its refresh token has expired");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{human_until, keychain_verdict, win_line};

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    #[test]
    fn human_until_formats_countdowns() {
        assert_eq!(human_until(1000, 900), "now", "past resets read as now");
        assert_eq!(human_until(0, 30), "0m", "sub-minute rounds down");
        assert_eq!(human_until(0, 2 * 3600 + 14 * 60), "2h 14m");
        assert_eq!(human_until(0, 3 * 86400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn win_line_shows_remaining_bar_and_reset() {
        let w = crate::quota::Window {
            used_pct: 61.0,
            resets_at: Some(2 * 3600 + 14 * 60),
        };
        let line = win_line("5h", &w, 0);
        assert!(line.contains("39% left"), "{line}");
        assert!(line.contains("resets in 2h 14m"), "{line}");
        let full = crate::quota::Window {
            used_pct: 0.0,
            resets_at: None,
        };
        let line = win_line("7d", &full, 0);
        assert!(line.contains("100% left"), "{line}");
        assert!(!line.contains("resets"), "no reset when absent: {line}");
    }

    const BARE: &str = "Claude Code-credentials";

    // The real-world multi-profile layout (one user, three CLAUDE_CONFIG_DIR
    // profiles): bare + two suffixed items, all LIVE logins.
    fn three_profiles() -> Vec<String> {
        s(&[
            BARE,
            "Claude Code-credentials-5953ba74",
            "Claude Code-credentials-feeb5ea6",
        ])
    }

    #[test]
    fn keychain_verdict_silent_when_no_item() {
        assert!(
            keychain_verdict(&[], Some(BARE), BARE).is_none(),
            "nothing to report if Claude has no Keychain item"
        );
    }

    #[test]
    fn keychain_verdict_manages_own_env_profile_among_aliased_siblings() {
        // No env -> swapdex manages the bare item; the suffixed items are
        // OTHER profiles (claude-work aliases) and must be called that - not
        // "stale strays" to delete.
        let (ok, msg) = keychain_verdict(&three_profiles(), Some(BARE), BARE).unwrap();
        assert!(ok, "coexisting aliased profiles are healthy: {msg}");
        assert!(msg.contains("other CLAUDE_CONFIG_DIR profiles"), "{msg}");
        assert!(msg.contains("never touches"), "{msg}");
    }

    #[test]
    fn keychain_verdict_single_profile_is_plain_ok() {
        let (ok, msg) = keychain_verdict(&s(&[BARE]), Some(BARE), BARE).unwrap();
        assert!(ok);
        assert!(msg.contains("managing this environment's profile"), "{msg}");
    }

    #[test]
    fn keychain_verdict_flags_refused_ambiguity() {
        // The derived item does not exist and several profiles are present:
        // resolution refuses to guess (target None) and doctor explains.
        let found = s(&[
            "Claude Code-credentials-5953ba74",
            "Claude Code-credentials-feeb5ea6",
        ]);
        let (ok, msg) = keychain_verdict(&found, None, BARE).unwrap();
        assert!(!ok, "refused ambiguity is a finding");
        assert!(msg.contains("refuses to guess"), "{msg}");
        assert!(msg.contains("CLAUDE_CONFIG_DIR"), "{msg}");
        assert!(msg.contains(BARE), "names the missing derived item: {msg}");
    }

    #[test]
    fn keychain_verdict_single_item_fallback_is_ok_with_note() {
        // Alias-only setup: env derives bare (missing), the only login is a
        // suffixed item - swapdex manages it, with a pointer to the env.
        let (ok, msg) = keychain_verdict(
            &s(&["Claude Code-credentials-5953ba74"]),
            Some("Claude Code-credentials-5953ba74"),
            BARE,
        )
        .unwrap();
        assert!(ok, "managing the only existing login works: {msg}");
        assert!(msg.contains("only"), "{msg}");
        assert!(msg.contains("CLAUDE_CONFIG_DIR"), "{msg}");
    }

    #[test]
    fn keychain_verdict_flags_target_not_in_found() {
        // Defensive: the two keychain reads disagreed.
        let (ok, msg) =
            keychain_verdict(&s(&["Claude Code-credentials-5953ba74"]), Some(BARE), BARE).unwrap();
        assert!(!ok);
        assert!(msg.contains("re-run"), "{msg}");
    }
}
