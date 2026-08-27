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

/// Is `cmd` on PATH? Answered by LOOKING, never by running it.
///
/// This used to shell out to `cmd --version`, which meant `swapdex doctor` -
/// whose whole job is to observe - executed whatever `claude` resolved to. With
/// the shim installed that is the shim, and the shim starts a proxy: a
/// diagnostic that silently launched a daemon every time it ran.
fn command_exists(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(cmd)))
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
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
        // Surface the captured identity so a stale oauthAccount (saving 'rnd'
        // while bsgong is the live account) is caught at save, not discovered
        // later when `use` connects the wrong account.
        let email = adapter
            .identity(paths)
            .ok()
            .flatten()
            .and_then(|id| id.email);
        saved.push((tool, email));
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
    let saved_disp = saved
        .iter()
        .map(|(tool, email)| match email {
            Some(e) => format!("{tool} = {e}"),
            None => tool.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!("saved profile '{name}' ({saved_disp}){note}");
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

/// Whether a name-detectable, non-slotted tool (codex/gemini/antigravity) has a
/// live process - the rotation-logout guard input. A dry-run and SWAPDEX_ROOT
/// never scan real processes (an isolated root's credentials are not the ones a
/// real session uses); under SWAPDEX_ROOT a test hook names the tools to treat
/// as running, e.g. `SWAPDEX_TEST_RUNNING=codex,gemini`.
fn tool_session_running(tool: &str, running: &[String], dry_run: bool) -> bool {
    if dry_run {
        return false;
    }
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        return std::env::var("SWAPDEX_TEST_RUNNING")
            .map(|v| v.split(',').any(|t| t.trim() == tool))
            .unwrap_or(false);
    }
    crate::proc::tool_running(tool, running)
}

pub fn use_account(
    paths: &Paths,
    name: &str,
    sel: Option<ToolSel>,
    dry_run: bool,
    force: bool,
) -> Result<i32> {
    // Permanent-slot account: `use` just repoints the default pointer (the
    // claude shim follows it) - no credential copy, so no rotation logout. A
    // legacy copy-model profile (not in the slot registry) falls through to the
    // old guarded switch.
    let tool = slot_tool(sel);
    if crate::slots::Slots::open_for(paths, tool)?
        .get(name)
        .is_some()
    {
        return use_slot_default(paths, name, tool, dry_run);
    }
    use_account_inner(paths, name, sel, dry_run, false, None, force)
}

/// `open`: after a successful switch, exec the tool (the --open flag; needs an
/// explicit --tool so there is never a guess about WHICH conversation opens).
pub fn use_account_open(
    paths: &Paths,
    name: &str,
    sel: Option<ToolSel>,
    dir: Option<&std::path::Path>,
    force: bool,
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
    use_account_inner(paths, name, sel, false, true, dir, force)
}

fn use_account_inner(
    paths: &Paths,
    name: &str,
    sel: Option<ToolSel>,
    dry_run: bool,
    open: bool,
    open_dir: Option<&std::path::Path>,
    force: bool,
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
    // Pre-switch guard for Claude: swapping the login slot while a `claude`
    // session is using THAT slot both clobbers the incoming login AND revokes
    // the outgoing account's just-saved snapshot on the session's next token
    // refresh (Claude's refresh tokens rotate - the saved copy dies). Compute
    // the verdict once (a process scan); enforced per-tool below unless --force.
    // Skipped on a dry-run and under SWAPDEX_ROOT (no real sessions in play).
    let claude_guard = if dry_run {
        crate::proc::GuardVerdict::Clear
    } else if std::env::var_os("SWAPDEX_ROOT").is_some() {
        // Sandbox: never scan real processes (they are not the ones an isolated
        // root's credentials belong to). A test hook - honored ONLY here - can
        // inject a verdict to exercise the enforcement path.
        match std::env::var("SWAPDEX_TEST_CLAUDE_GUARD").ok().as_deref() {
            Some("same-slot") => crate::proc::GuardVerdict::SameSlot,
            Some("unknown") => crate::proc::GuardVerdict::Unknown,
            _ => crate::proc::GuardVerdict::Clear,
        }
    } else {
        crate::proc::claude_switch_guard(
            std::env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .ok()
                .as_deref(),
            std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
            &crate::proc::running_claude_procs(),
        )
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
        // Pre-switch guard (Claude only): refuse to swap the slot while a claude
        // session is using it, or when a running session's slot is unknown
        // (fail closed). Doing so would clobber the incoming login and, on the
        // session's next refresh, revoke the outgoing account's snapshot ->
        // a later switch-back logs it out. `--force` overrides.
        if tool == "claude-code" && !force {
            match claude_guard {
                crate::proc::GuardVerdict::SameSlot => {
                    eprintln!(
                        "swapdex: {tool}: a Claude session is running on THIS login slot. \
                         Switching now would log that account out on its next token refresh \
                         (the refresh token rotates and the saved copy is revoked). Quit that \
                         `claude` and retry, or `swapdex use {name} --tool claude --force` to \
                         switch anyway."
                    );
                    failed.push(tool);
                    continue;
                }
                crate::proc::GuardVerdict::Unknown => {
                    eprintln!(
                        "swapdex: {tool}: a Claude session is running but swapdex could not read \
                         which login slot it uses, so it can't rule out that switching would log \
                         it out. Quit `claude` and retry, or `--force` to switch anyway."
                    );
                    failed.push(tool);
                    continue;
                }
                crate::proc::GuardVerdict::Clear => {}
            }
        }
        // Pre-switch rotation-logout guard for the NON-slotted tools (codex,
        // gemini, antigravity), the running-session analog of Claude's guard.
        // They all rotate their OAuth token on refresh, and swapdex swaps their
        // shared credential files - so switching while the tool runs lets that
        // session's next refresh revoke the account being switched, logging it
        // out (e.g. a `codex` MCP server used from Claude). Claude is slot-
        // isolated and guarded above. Detection is by process name (no slot);
        // --force overrides.
        if matches!(tool, "codex" | "gemini" | "antigravity")
            && !force
            && tool_session_running(tool, &running, dry_run)
        {
            eprintln!(
                "swapdex: {tool}: a running {tool} session was detected. {tool} rotates its \
                 OAuth token on refresh, so switching now can log that account out on the \
                 session's next refresh. Quit it and retry, or `swapdex use {name} --tool \
                 {} --force` to switch anyway.",
                pretty_tool_flag(tool)
            );
            failed.push(tool);
            continue;
        }
        // Exclude a concurrent `login` mid-sign-in on THIS tool: it holds the
        // per-tool credential lock across its interactive wait (while the store
        // lock is free), so switching now would race its sign-out/capture. Skip
        // this tool, keep switching the others. Held for the rest of this tool's
        // credential work.
        let _cred_lock = match store.lock_tool(tool) {
            Ok(g) => g,
            Err(_) => {
                eprintln!(
                    "swapdex: {tool}: a `swapdex login` is signing this tool in right now; \
                     skipped. Retry after it finishes."
                );
                failed.push(tool);
                continue;
            }
        };
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
        // Same per-tool credential lock as `use`: a `login` mid-sign-in on this
        // tool holds it across its interactive wait, so restoring now would race
        // its sign-out/capture. Skip this tool, keep restoring the others.
        let _cred_lock = match store.lock_tool(tool) {
            Ok(g) => g,
            Err(_) => {
                eprintln!(
                    "swapdex: {tool}: a `swapdex login` is signing this tool in right now; \
                     skipped. Retry after it finishes."
                );
                continue;
            }
        };
        // Capture the CURRENT login first, but do NOT back it up yet: if
        // apply(target) fails, backing it up now would make it the NEWEST backup,
        // and a retry would see it as "already active" - stranding the very
        // backup we were asked to restore. Promote it only after apply succeeds.
        let live_snap = if adapter.present(paths) {
            match adapter.capture(paths) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!(
                        "swapdex: note - the current {tool} login could not be read ({e:#}); \
                         restoring without a backup of it"
                    );
                    None
                }
            }
        } else {
            None
        };
        adapter.apply(paths, &target)?;
        // apply(target) succeeded: NOW it is safe to record the outgoing login as
        // a backup (so `restore` toggles back) and refresh its profile(s) with
        // its freshest tokens - the same rotation invariant as `use`.
        if let Some(live_snap) = &live_snap {
            store.backup(live_snap)?;
            if let Some(id) = &live_id {
                for pname in matching_profile_names(&store, tool, id) {
                    store.save(&pname, live_snap)?;
                }
            }
        }
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

/// What to DO about the stale tools, and what still works despite them.
///
/// Naming the stale tool was half the job. The old hint said to re-run
/// `add --update`, which re-saves a snapshot FROM a live login - no help when
/// the login itself is what lapsed. The tool has to be signed into first, and
/// that is per-tool.
///
/// It also says which tools still serve. A lone marker beside an account reads
/// as "this account is broken" when Claude and Codex are working perfectly
/// well - which was the actual confusion it caused.
///
/// Empty when nothing is stale. When EVERYTHING is stale it promises nothing,
/// because there is nothing to reassure anyone about.
pub fn stale_hint(stale: &[&str], healthy: &[&str]) -> String {
    if stale.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "  ({}: run that tool once and sign in - re-saving the profile cannot \
         refresh a login that has already lapsed)",
        stale.join(", ")
    );
    if !healthy.is_empty() {
        out.push_str(&format!(
            "\n  (the account still serves {} - a stale tool does not hold the others back)",
            healthy.join(", ")
        ));
    }
    out
}

/// The payer's remaining quota, short enough for a status bar.
///
/// Going through the proxy costs the tool its own `rate_limits` block, so a
/// status line that reads them prints "weekly N/A | 5h N/A" while swapdex holds
/// a reading from a minute ago. This renders that reading from cache: a bar
/// redraws constantly and cannot wait on a request.
///
/// Takes USED percentages and reports what is LEFT. Empty when nothing has been
/// measured, so the bar can omit the segment rather than print a placeholder
/// that looks like a reading.
/// How stale the status bar's number is, in the fewest characters that say it.
///
/// The bar reads the cache, which records when each number was taken, and threw
/// that away - so a reading taken before an account's window filled sat there
/// saying "5h 100%" while the account was refusing every turn. The bar is
/// refreshed about once a minute, so past ten minutes the readings have stopped
/// arriving, and that is worth a few characters.
fn bar_age(age_secs: i64, refresh_secs: i64) -> Option<String> {
    // Late means late for THIS account's own schedule. An account with plenty
    // left is re-read every fifteen minutes by design, so a flat ten-minute
    // threshold flagged it while everything was working - and a warning that
    // shows when nothing is wrong is one the reader learns to skip. Twice its
    // own interval means a refresh was due and did not come; the ten-minute
    // floor keeps a once-a-minute account from being nagged about at two.
    let overdue = refresh_secs.saturating_mul(2).max(600);
    if age_secs < overdue {
        return None;
    }
    let m = age_secs / 60;
    Some(if m < 60 {
        format!(" · {m}m old")
    } else {
        format!(" · {}h old", m / 60)
    })
}

pub fn quota_brief(five_h_used: Option<f64>, seven_d_used: Option<f64>) -> String {
    let left = |u: f64| (100.0 - u).clamp(0.0, 100.0);
    match (five_h_used, seven_d_used) {
        (Some(a), Some(b)) => format!("5h {:.0}% | 7d {:.0}%", left(a), left(b)),
        (Some(a), None) => format!("5h {:.0}%", left(a)),
        (None, Some(b)) => format!("7d {:.0}%", left(b)),
        // Saying nothing is indistinguishable from a broken status bar. The
        // numbers vanish for two ordinary reasons - nothing measured yet, and a
        // window that passed its reset before a fresh read landed - and in both
        // the honest answer is that there is no reading, not blank space.
        (None, None) => "usage unread".to_string(),
    }
}

/// How to sign an account in, naming the tool it is about.
///
/// A Codex account that could not serve was told to run `swapdex run
/// codex-test`, and `run` defaults to Claude - so following the instruction
/// launched Claude for a Codex profile. Advice that names the wrong tool is
/// worse than none.
pub fn sign_in_remedy(name: &str, tool: &str) -> String {
    // Claude is `run`'s default; naming it would be noise.
    let flag = match tool {
        "claude-code" | "claude" => String::new(),
        other => format!(" --tool {other}"),
    };
    format!("`swapdex run {name}{flag}` signs it in once")
}

/// What to say when a name matches no account.
///
/// A typo answered "no account named 'alicee' - `swapdex ui` lists them",
/// sending the user to open another screen to read four words. The list is
/// right here; and when one candidate is an obvious near-miss, naming it is the
/// whole answer.
pub fn unknown_account_or_unservable(
    asked: &str,
    servable: &[&str],
    saved: &[&str],
    tool: &str,
) -> String {
    // A name that IS saved but cannot serve is a different problem, and saying
    // "no account named X - you have: X" is worse than useless. Serving reads a
    // slot's own credential directory; a snapshot has none until it is run once.
    if saved.contains(&asked) && !servable.contains(&asked) {
        return format!(
            "'{asked}' is saved but has never been signed in on this machine, so it \
             cannot pay for turns - {}",
            sign_in_remedy(asked, tool)
        );
    }
    // Nothing can serve, but something IS saved: "no accounts saved yet" would
    // be a plain untruth about accounts `ls` is showing. Name them and say what
    // they still need.
    if servable.is_empty() && !saved.is_empty() {
        let flag = if tool == "claude-code" {
            String::new()
        } else {
            format!(" --tool {tool}")
        };
        return format!(
            "no account named '{asked}'. Saved, but not yet signed in on this machine: \
             {} - `swapdex run <name>{flag}` signs one in",
            saved.join(", ")
        );
    }
    unknown_account(asked, servable)
}

pub fn unknown_account(asked: &str, known: &[&str]) -> String {
    if known.is_empty() {
        return "no accounts saved yet - `swapdex add <name>` saves the login you are on"
            .to_string();
    }
    // A near-miss is a typo: one edit away, or one is a prefix of the other.
    let near = known.iter().find(|k| {
        let (a, b) = (asked.to_lowercase(), k.to_lowercase());
        a != b && (a.starts_with(&b) || b.starts_with(&a)) && a.len().abs_diff(b.len()) <= 2
    });
    match near {
        Some(hit) => {
            let others: Vec<&str> = known.iter().copied().filter(|k| k != hit).collect();
            if others.is_empty() {
                format!("no account named '{asked}' - did you mean '{hit}'?")
            } else {
                format!(
                    "no account named '{asked}' - did you mean '{hit}'? (also: {})",
                    others.join(", ")
                )
            }
        }
        None => format!(
            "no account named '{asked}' - you have: {}",
            known.join(", ")
        ),
    }
}

/// One line confirming a switch: who is paying now, and what they have left.
///
/// `serve` printed the destination and two lines of explanation, and said
/// nothing about the account itself - so every switch was followed by `ls` to
/// see whether it took, and by `usage` to see whether that account had any
/// room. Both answers belong in the switch that prompted them.
///
/// What is unknown is left unsaid: an account whose window has never been read
/// gets its name and nothing more, rather than a percentage nobody measured.
pub fn switch_line(name: &str, email: Option<&str>, week_left_pct: Option<f64>) -> String {
    let who = match email {
        Some(e) => format!("now {name} ({e})"),
        None => format!("now {name}"),
    };
    match week_left_pct {
        // Spent reads worse as "0% left" than as plain words.
        Some(p) if p < 0.5 => format!("{who} - no week left"),
        Some(p) => format!("{who} - {:.0}% of the week left", p),
        None => who,
    }
}

/// Whose account this row is, preferring what the snapshot recorded.
///
/// Identity was read from the saved snapshot only, so an account that exists as
/// a slot and was never snapshotted had an empty name column - the row was
/// there and switching worked, but nothing said which login it was. The slot's
/// own `.claude.json` answers when the snapshot cannot.
///
/// The snapshot wins when both speak: it is what this profile was SAVED as,
/// and a slot's config can be overwritten by whatever last ran in it.
pub fn best_identity(from_snapshot: Option<String>, from_slot: Option<String>) -> Option<String> {
    from_snapshot.or(from_slot)
}

/// Every account that can be switched to, snapshots and slots together.
///
/// `ls` listed saved snapshots only. On a machine whose accounts live as SLOTS,
/// `serve personal` moved the turns correctly and the list had no row for
/// `personal` at all - so the mark saying who pays had nowhere to appear, and
/// two of three switches looked like they did nothing.
///
/// Sorted and de-duplicated: an account with both a slot and a snapshot is one
/// account, and a stable order keeps the table from reshuffling between runs.
pub fn listable(snapshots: &[&str], slots: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = snapshots
        .iter()
        .chain(slots.iter())
        .map(|s| s.to_string())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Who is paying, across tools.
///
/// `ls` asked Claude's registry alone, so serving a Codex account moved the
/// turns correctly and the listing marked nobody - the same "the switch did
/// nothing" appearance fixed for Claude in 0.80.0, still live on the Codex
/// side. Claude wins when both have a payer: one mark can only carry one name,
/// and Claude is the tool the rest of the row is about.
pub fn payer_of_any(per_tool: &[(&str, Option<&str>)]) -> Option<String> {
    let pick = |want: &str| {
        per_tool
            .iter()
            .find(|(t, _)| *t == want)
            .and_then(|(_, p)| p.map(str::to_string))
    };
    pick("claude-code").or_else(|| per_tool.iter().find_map(|(_, p)| p.map(str::to_string)))
}

/// The mark a single row gets for paying, if any.
///
/// `serve rnd` says "turns -> rnd", and then `ls` starred a different account -
/// the one holding the login on disk - with the paying account named nowhere.
/// Switching looked like it had not taken, which is what its owner concluded,
/// repeatedly, over a day.
///
/// `None` when there is nothing to disambiguate: same account, or nothing
/// paying. An extra mark on the common case is noise.
pub fn row_suffix(
    row: &str,
    _signed_in: Option<&str>,
    paying: Option<&str>,
) -> Option<&'static str> {
    // Marked even when the payer also holds the login. Suppressing it there
    // meant that of three accounts, switching to one produced no visible
    // change at all, which reads as that one switch having failed.
    (row == paying?).then_some("pays")
}

/// Who pays, when that is not who the tool is signed in as.
///
/// `ls` marks the account Claude holds a login for. When a proxy is paying with
/// a different one, that mark is true and misleading at once: the owner
/// switched to bsgong, the proxy served every turn from bsgong, and the list
/// went on starring kong because that is whose login sits on disk. Both facts
/// are real; showing only one reads as the switch having failed.
///
/// `None` when there is nothing to disambiguate - same account, or nothing
/// paying - because an extra note on the common case is just noise.
pub fn payer_note(signed_in: Option<&str>, paying: Option<&str>) -> Option<String> {
    let paying = paying?;
    if signed_in == Some(paying) {
        return None;
    }
    Some(format!("{paying} pays"))
}

/// The staleness note for a profile, naming the tools it is about.
///
/// One tool going stale is not the account going stale. A profile holding four
/// logins was marked `(stale)` whole because gemini had not been refreshed in
/// 37 days, while the Codex login added minutes earlier answered the server
/// perfectly well - so the row said "unusable" about an account that worked,
/// and the owner reasonably read it as the add having failed.
///
/// `None` when nothing is wrong, which is the common case and needs no words.
pub fn stale_marker(per_tool: &[(&str, Option<&str>)]) -> Option<String> {
    let named: Vec<String> = per_tool
        .iter()
        .filter_map(|(tool, m)| m.map(|m| format!("{tool} {m}")))
        .collect();
    (!named.is_empty()).then(|| named.join(", "))
}

/// Summarize a profile across ALL its tools (not just the first): a marker if
/// ANY tool is stale/expired, and the first non-empty email/tier. `p.tools` is
/// alphabetical, so inspecting only the first would always be "claude-code" and
/// hide Codex entirely.
fn profile_summary(
    store: &Store,
    name: &str,
    tools: &[String],
) -> (Option<String>, Option<String>, Option<String>) {
    let mut email = None;
    let mut tier = None;
    let mut per_tool: Vec<(&str, Option<&str>)> = Vec::new();
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
            // Recorded per tool, so the note can say WHICH login is stale
            // rather than condemning the account.
            per_tool.push((t, m));
        }
    }
    (email, tier, stale_marker(&per_tool))
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

    let mut profiles = store.list();
    let mut slot_dirs: std::collections::HashMap<String, std::path::PathBuf> =
        std::collections::HashMap::new();
    // Slots are switchable too. `ls` listed saved snapshots only, so on a
    // machine whose accounts live as slots, `serve personal` moved the turns
    // correctly and there was no row for `personal` to mark - two of three
    // switches looked like they did nothing.
    // A registry that will not parse is not an empty one. Until 0.103.0 this
    // file was written with a plain truncate-then-write, so an interrupted
    // write could leave it unreadable - and reporting that as "no accounts"
    // tells someone whose credentials are all still on disk to start over.
    let mut unreadable_registry: Vec<&str> = Vec::new();
    for tool in crate::adapters::names() {
        if let Err(e) = crate::slots::Slots::open_for(paths, tool) {
            let _ = e;
            unreadable_registry.push(tool);
        }
        if let Ok(sl) = crate::slots::Slots::open_for(paths, tool) {
            for r in sl.list() {
                match profiles.iter_mut().find(|p| p.name == r.name) {
                    Some(p) => {
                        if !p.tools.iter().any(|t| t == tool) {
                            p.tools.push(tool.to_string());
                        }
                    }
                    None => profiles.push(crate::store::ProfileInfo {
                        name: r.name.clone(),
                        tools: vec![tool.to_string()],
                    }),
                }
                // Remember where this slot lives, so a row with no snapshot can
                // still say whose login it is - the slot's own .claude.json
                // knows, and an empty name column left switching unverifiable.
                slot_dirs
                    .entry(r.name.clone())
                    .or_insert_with(|| r.config_dir.clone());
            }
        }
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    if json {
        let rows: Vec<Value> = profiles
            .iter()
            .map(|p| {
                let (email, tier, marker) = profile_summary(&store, &p.name, &p.tools);
                // A slot-only account has no snapshot to name it; its own config
                // does. Without this the row appeared with an empty name column, so
                // a switch to it could not be checked against anything.
                let email = best_identity(
                    email,
                    slot_dirs
                        .get(&p.name)
                        .and_then(|d| crate::proxy::creds::any_slot_email(d)),
                );
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
    if !unreadable_registry.is_empty() {
        eprintln!(
            "swapdex: the account registry could not be read ({}) - \
             ~/.local/share/swapdex/slots.json is damaged, so accounts are missing \
             from this listing. The slot directories still hold their logins; \
             restore the file from a backup or re-register with `swapdex adopt`.",
            unreadable_registry.join(", ")
        );
    }
    if profiles.is_empty() {
        if unreadable_registry.is_empty() {
            println!("No accounts saved yet.");
            println!("  guided setup:  swapdex setup");
            println!("  or add one:    swapdex login <name>");
        }
        return Ok(0);
    }
    // Two-pass so columns fit the actual content (with a sane cap).
    struct Row {
        name: String,
        ident: String,
        tools: String,
        warn: Option<String>,
        active: bool,
        /// Set when this row is the account PAYING and a different one holds
        /// the login. `serve` said "turns -> rnd" and the list starred another
        /// name, so the switch read as not having taken.
        pays: bool,
    }
    // Same resolution the proxy performs, so what this screen claims and what
    // the proxy does cannot drift apart.
    // Ask EVERY tool who pays. Asking Claude's registry alone meant serving a
    // Codex account moved the turns and the listing marked nobody - the same
    // "the switch did nothing" appearance fixed for Claude in 0.80.0.
    let payers: Vec<(&str, Option<String>)> = crate::adapters::names()
        .into_iter()
        .map(|t| {
            (
                t,
                crate::slots::Slots::open_for(paths, t)
                    .ok()
                    .and_then(|s| s.payer()),
            )
        })
        .collect();
    let refs: Vec<(&str, Option<&str>)> = payers.iter().map(|(t, p)| (*t, p.as_deref())).collect();
    let paying = payer_of_any(&refs);
    let signed_in = active_by_tool(&store, paths)
        .into_iter()
        .find(|(t, _)| *t == "claude-code")
        .map(|(_, n)| n);
    let rows: Vec<Row> = profiles
        .iter()
        .map(|p| {
            let (email, tier, marker) = profile_summary(&store, &p.name, &p.tools);
            // A slot-only account has no snapshot to name it; its own config
            // does. Without this the row appeared with an empty name column, so
            // a switch to it could not be checked against anything.
            let email = best_identity(
                email,
                slot_dirs
                    .get(&p.name)
                    .and_then(|d| crate::proxy::creds::any_slot_email(d)),
            );
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
            // An expired slot cannot serve, and the proxy says so at the time
            // ("its login has expired - passing your own login through"). The
            // TUI marked it; this listing showed the row with no note at all,
            // presenting an unusable account as fine - and `ls` is what a
            // script and a glance both read.
            let expired = slot_dirs
                .get(&p.name)
                .is_some_and(|d| crate::proxy::creds::slot_token_expired(d, now_ms()));
            let marker = match (marker, expired) {
                (Some(m), _) => Some(m),
                (None, true) => Some("expired".to_string()),
                (None, false) => None,
            };
            Row {
                pays: row_suffix(&p.name, signed_in.as_deref(), paying.as_deref()).is_some(),
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
    // Which tools are stale, and which of that account's tools still serve.
    // A lone marker reads as "this account is broken" when the others work.
    let mut stale_tools: Vec<String> = Vec::new();
    let mut healthy_tools: Vec<String> = Vec::new();
    for r in &rows {
        let mark = if r.active { "* " } else { "  " };
        let warn = r
            .warn
            .as_deref()
            .map(|m| format!("  ({m})"))
            .unwrap_or_default();
        saw_unreadable |= r.warn.as_deref() == Some("unreadable");
        // The note now names the tool ("gemini stale"), so match on the
        // word rather than the whole string.
        saw_refreshable |= r
            .warn
            .as_deref()
            .is_some_and(|w| w.contains("expired") || w.contains("stale"));
        if let Some(w) = r.warn.as_deref() {
            for t in r.tools.split(',').map(|t| t.trim().trim_end_matches('*')) {
                if t.is_empty() {
                    continue;
                }
                // The note already names the tools ("gemini stale"), so it is
                // the authority on which side each one falls.
                let bucket = if w.contains(t) {
                    &mut stale_tools
                } else {
                    &mut healthy_tools
                };
                if !bucket.iter().any(|x| x == t) {
                    bucket.push(t.to_string());
                }
            }
        }
        // The paying account is named on its own row: `serve` moved the turns
        // there, and without this the list starred a different name and the
        // switch read as not having taken.
        let pays = if r.pays { "  <- pays" } else { "" };
        println!(
            "{mark}{} {} [{}]{warn}{pays}",
            fit(&r.name, name_w),
            fit(&r.ident, ident_w),
            r.tools
        );
    }
    if saw_refreshable {
        let stale: Vec<&str> = stale_tools.iter().map(String::as_str).collect();
        let healthy: Vec<&str> = healthy_tools.iter().map(String::as_str).collect();
        let hint = stale_hint(&stale, &healthy);
        if hint.is_empty() {
            println!(
                "  (expired/stale: run that tool once and sign in - re-saving the profile \
                 cannot refresh a login that has already lapsed)"
            );
        } else {
            println!("{hint}");
        }
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
/// What to say when `short_line` had nothing to report.
///
/// "Nothing is signed in" and "the logins cannot be read from here" are
/// different news, and a blank line says neither. A locked macOS Keychain over
/// SSH produces the second, and reporting it as the first is how a working
/// account gets called signed out.
fn absence_reason(any_unreadable: bool) -> &'static str {
    if any_unreadable {
        "logins unreadable from this shell"
    } else {
        "not signed in to any tool"
    }
}

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
        println!(
            "{}",
            short_line(paths).unwrap_or_else(|| {
                let unreadable = adapters::all().iter().any(|a| a.identity(paths).is_err());
                absence_reason(unreadable).to_string()
            })
        );
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

/// `proxy` - run proxy mode in the foreground. Claude Code pointed at it
/// (`ANTHROPIC_BASE_URL`) gets its account chosen per request, so a RUNNING
/// conversation can change accounts without a restart or a resume.
#[allow(clippy::too_many_arguments)]
pub fn proxy(
    paths: &Paths,
    port: u16,
    account: Option<String>,
    sel: Option<ToolSel>,
    auto: bool,
    no_auto: bool,
    ensure: bool,
    threshold: Option<f64>,
) -> Result<i32> {
    if ensure {
        return proxy_ensure(paths, port, slot_tool(sel));
    }
    // A flag decides THIS run; no flag means "whatever the setting says", and the
    // proxy re-reads that on every request - so `swapdex auto on` reaches one that
    // is already running rather than waiting for a restart nobody performs.
    let auto = match (auto, no_auto) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    // A threshold means "step off before the wall", which needs one usage read per
    // account; opt-in, so the proxy originates no traffic unless asked.
    let threshold_pinned = threshold.is_some();
    let opts = crate::proxy::Opts {
        port,
        account,
        tool: slot_tool(sel).to_string(),
        auto,
        threshold: threshold.map(|t| t.clamp(0.05, 1.0)),
        threshold_pinned,
    };
    crate::proxy::serve(paths, &opts)?;
    Ok(0)
}

/// The port `swapdex proxy` listens on unless told otherwise. Codex's proxy
/// takes the next one, so both tools' shims can start their own.
pub const DEFAULT_PROXY_PORT: u16 = 8787;

/// The port a tool's proxy binds when nobody names one. Codex takes the next
/// port so both can run at once - the rule `proxy --ensure` has always applied,
/// now stated once so anything else writing a proxy invocation agrees with it.
pub fn default_port_for(tool: &str) -> u16 {
    if tool == "codex" {
        DEFAULT_PROXY_PORT + 1
    } else {
        DEFAULT_PROXY_PORT
    }
}

/// `proxy --ensure` - print the port of a live proxy, starting one in the
/// background if there is none. This is what lets a plain `claude` (through the
/// shim) get proxy mode without the user running or remembering anything. Exits
/// non-zero and prints nothing when a proxy cannot be had, so the shim simply
/// runs Claude directly.
fn proxy_ensure(paths: &Paths, port: u16, tool: &str) -> Result<i32> {
    // A hermetic root is a sandbox, and the proxy is deliberately DETACHED so it
    // outlives the shell that asked for it. Under a temporary store that is wrong
    // twice: the daemon outlives the store it was pointed at, and it keeps the
    // port - so a run leaves a listener on 127.0.0.1 answering for a directory
    // that no longer exists. One was found still bound hours after its store had
    // been deleted.
    // Two proxies cannot share a port. Codex takes the next one, so the shim for
    // either tool can start its own without asking the user to pick.
    let mut port = if tool == "codex" && port == DEFAULT_PROXY_PORT {
        port + 1
    } else {
        port
    };
    if let Some((pid, running, build)) = crate::proxy::running_proxy_for(paths, tool) {
        if build == crate::proxy::build_id() {
            println!("{running}");
            return Ok(0);
        }
        // A proxy from an older build is still answering: updating swapdex does
        // not update what is already running, so a fix can be installed, verified,
        // and still not be what serves the next request. Replace it on the SAME
        // port, because sessions already point at that port and would otherwise
        // be left talking to nothing.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        for _ in 0..40 {
            if crate::proxy::running_proxy_for(paths, tool).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        port = running;
    }
    // Proxy mode is only useful with slot accounts; without one there is nothing
    // to serve and starting a proxy would just add a moving part.
    if crate::slots::Slots::open_for(paths, tool)
        .map(|s| s.list().is_empty())
        .unwrap_or(true)
    {
        return Ok(1);
    }
    let Ok(exe) = std::env::current_exe() else {
        return Ok(1);
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("proxy")
        .arg("--port")
        .arg(port.to_string())
        .arg("--tool")
        .arg(tool)
        .stdin(std::process::Stdio::null());
    // Send its voice to a file rather than /dev/null. Discarding it meant that
    // on a machine where the shim starts the proxy, nothing recorded which
    // account served which turn - and that silence made a real switching bug
    // undiagnosable: three wrong conclusions before the log was added by hand.
    let log = paths.proxy_log(tool);
    let piped = std::fs::create_dir_all(log.parent().unwrap_or(&log))
        .ok()
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .ok()
        });
    // A log we cannot open must not stop the proxy from starting: being unable
    // to record is bad, being unable to serve is worse.
    match piped.and_then(|f| f.try_clone().ok().map(|e| (f, e))) {
        Some((out, err)) => {
            cmd.stdout(std::process::Stdio::from(out))
                .stderr(std::process::Stdio::from(err));
        }
        None => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
    }
    // Detach: the proxy must outlive this short-lived helper and the shell that
    // started it, and must never take the terminal (it would fight `claude` for
    // stdin) or die with the session.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    if cmd.spawn().is_err() {
        return Ok(1);
    }
    // Wait briefly for it to announce itself; a proxy that cannot bind must not
    // hang the launch of Claude.
    for _ in 0..40 {
        if let Some((_, p, _)) = crate::proxy::running_proxy_for(paths, tool) {
            println!("{p}");
            return Ok(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(1)
}

/// The instructions both assistants get. Kept deliberately small: run one swapdex
/// command and report what happened, so switching accounts does not mean leaving
/// the conversation for another terminal.
/// The switcher's instructions, written for ONE tool. A switcher offered inside
/// Claude that lists Codex accounts (or switches one) is answering a question
/// nobody asked - each assistant should only ever see, and only ever move, its
/// own accounts.
fn slash_body(tool: &str, host: &str) -> String {
    format!(
        "**If arguments were given**, run `swapdex use $ARGUMENTS --tool {tool}`, then \
         report the result in one line.\n\
         \n\
         **If not**, do not make the user recall account names:\n\
         \n\
         1. Run `swapdex ls` and keep ONLY the accounts tagged `{tool}` - this is {host}, \
         so accounts for other tools are not offered and never switched.\n\
         2. Ask the user to choose one with the AskUserQuestion tool, so they can pick with \
         the arrow keys. One option per account, labelled with the account name, with its \
         email and current state as the description. Put the active one first and say it is \
         active. If there are more accounts than the tool allows, offer the ones not \
         currently active and let the rest come from free text.\n\
         3. Run `swapdex serve <the account they chose> --tool {tool}` and report the \
         result in one line. `serve` is the right verb here: it changes which account \
         pays for the turns and leaves this conversation exactly where it is. `use` \
         would move the store the conversation lives in, which is never what someone \
         asking mid-conversation means.\n\
         \n\
         Report what swapdex printed and nothing beyond it. Its last line says whether \
         anything running actually moved: with a proxy the next turn of THIS session is \
         served by the new account, and without one the change reaches only the next \
         launch while this conversation keeps the account it began with. Do not promise \
         the stronger of the two - a switch announced as live when it was not is worse \
         than no switch, because the work continues on the account the user thinks they \
         left. If the output says the shim is not taking effect, say so and stop.\n"
    )
}

/// `slash` - install the Claude Code slash command, so an account switch can be
/// typed into the conversation instead of another terminal.
/// The Claude Code form: a command file with the frontmatter its picker reads.
fn claude_command_body() -> String {
    format!(
        "---\ndescription: Switch the Claude account serving this session (swapdex)\n---\n\n{}",
        slash_body("claude-code", "Claude Code")
    )
}

/// The Codex form: a skill, with the frontmatter Codex reads.
fn codex_skill_body() -> String {
    format!(
        "---\nname: swap\ndescription: >-\n  Switch the Codex account serving this session \
         (swapdex). Use when the user asks to change accounts, says an account is out of \
         quota, or types /swap.\n---\n\n{}",
        slash_body("codex", "Codex")
    )
}

/// `threshold [<fraction>|off]` - read or set the point at which the proxy steps
/// off an account. A setting rather than a flag because the proxy the shim starts
/// takes no flags, and that is the one doing the work day to day.
pub fn threshold(paths: &Paths, value: Option<&str>) -> Result<i32> {
    let cfg = crate::settings::load(paths);
    let Some(value) = value else {
        match cfg.threshold() {
            Some(t) => println!(
                "stepping off an account at {:.0}% used",
                (t * 100.0).round()
            ),
            None => println!(
                "no threshold - the proxy waits for an account to refuse a turn \
                 (`swapdex threshold 0.9` steps off earlier)"
            ),
        }
        return Ok(0);
    };
    let v = value.trim();
    if v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none") {
        crate::settings::update(paths, |c| c.proxy_threshold = None)?;
        println!("threshold off - the proxy waits for a refusal before moving");
        return Ok(0);
    }
    // Accept "0.9" and "90%"/"90" alike: both are how people say this.
    let parsed =
        v.trim_end_matches('%')
            .parse::<f64>()
            .ok()
            .map(|n| if n > 1.0 { n / 100.0 } else { n });
    let Some(t) = parsed.filter(|t| *t > 0.0 && *t <= 1.0) else {
        eprintln!(
            "swapdex: expected a fraction like 0.9, a percentage like 90%, or `off` - got '{v}'"
        );
        return Ok(2);
    };

    crate::settings::update(paths, |c| c.proxy_threshold = Some(t))?;
    // Report what was just stored, not the value read before the write: `cfg`
    // is the pre-edit snapshot and would print the OLD threshold back.
    let eff = t;
    println!(
        "stepping off an account at {:.0}% used - it hands the session on before \
         being refused",
        (eff * 100.0).round()
    );
    Ok(0)
}

/// `slash` - install the in-conversation switcher for both assistants, so an
/// account change can be typed where you already are rather than in another
/// terminal. Claude Code reads `~/.claude/commands`, Codex reads `~/.codex/skills`.
/// Hand the conversation you were just in to an account that still has room.
///
/// The proxy already moves a RUNNING session between accounts. This is the
/// other case: the turn is over, you are out, and the conversation lives in one
/// account's store. Under swapdex's slot model each account has its own
/// `CLAUDE_CONFIG_DIR` with its own `projects/`, so continuing elsewhere means
/// carrying the transcript across that boundary - which is sessionwiki's job,
/// and why this command orchestrates rather than implements.
///
/// Nothing here is silent. Each step says what it did, and a step that could
/// not run says so rather than letting the next one look like it worked.
/// Make every conversation reachable from every account.
///
/// Slots created before transcripts were shared keep their own `projects/`, so
/// a conversation started on one account is invisible from the others. This
/// carries those conversations into the shared store and points the slot at it.
///
/// Nothing is deleted. The slot's old directory is renamed aside, not removed,
/// because it holds real conversations and a rename is undoable while a delete
/// is not.
pub fn share_history(paths: &Paths, tool: &str, dry_run: bool) -> Result<i32> {
    let bare = match tool {
        "codex" => paths.codex_dir().to_path_buf(),
        _ => paths.claude_dir().to_path_buf(),
    };
    let dir_name = if tool == "codex" {
        "sessions"
    } else {
        "projects"
    };
    let shared = bare.join(dir_name);

    let slots = crate::slots::Slots::open_for(paths, tool)
        .map(|s| s.list())
        .unwrap_or_default();
    let mut touched = 0;
    for r in slots {
        let own = r.config_dir.join(dir_name);
        if own == shared {
            continue;
        }
        if std::fs::symlink_metadata(&own).is_ok_and(|m| m.file_type().is_symlink()) {
            continue; // already shared
        }
        if !own.is_dir() {
            // Never linked and nothing of its own: just point it at the shared
            // store so the next session lands somewhere everyone can see.
            if !dry_run {
                std::fs::create_dir_all(&shared)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&shared, &own).ok();
            }
            println!("  {} - linked to the shared history", r.name);
            touched += 1;
            continue;
        }
        let carried = crate::slots::carry_history_into_shared(&own, &shared, dry_run)?;
        println!(
            "  {} - {carried} conversation(s) only it had, carried over",
            r.name
        );
        if !dry_run {
            let aside = r.config_dir.join(format!("{dir_name}.before-sharing"));
            if aside.exists() {
                anyhow::bail!(
                    "{} already has {} - move it away first; nothing was changed for this account",
                    r.name,
                    aside.display()
                );
            }
            std::fs::rename(&own, &aside)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&shared, &own).ok();
            println!("    its old copy kept at {}", aside.display());
        }
        touched += 1;
    }
    if touched == 0 {
        println!("every account already shares its history - nothing to do");
    } else if dry_run {
        println!("(dry run - nothing changed)");
    } else {
        println!("done - every conversation is now reachable from every account");
    }
    Ok(0)
}

pub fn install_slash(paths: &Paths) -> Result<i32> {
    let _ = paths; // these dirs belong to the assistants, not swapdex's store
    let Some(home) = dirs::home_dir() else {
        eprintln!("swapdex: cannot find your home directory");
        return Ok(1);
    };
    let mut installed = 0;

    let claude_dir = home.join(".claude").join("commands");
    match std::fs::create_dir_all(&claude_dir)
        .and_then(|()| std::fs::write(claude_dir.join("swap.md"), claude_command_body()))
    {
        Ok(()) => {
            println!(
                "Claude Code: /swap  ({})",
                crate::util::redact_path(&claude_dir.join("swap.md").display().to_string())
            );
            installed += 1;
        }
        Err(e) => eprintln!("swapdex: could not install the Claude command: {e}"),
    }

    let codex_dir = home.join(".codex").join("skills").join("swap");
    match std::fs::create_dir_all(&codex_dir)
        .and_then(|()| std::fs::write(codex_dir.join("SKILL.md"), codex_skill_body()))
    {
        Ok(()) => {
            println!(
                "Codex:       /swap  ({})",
                crate::util::redact_path(&codex_dir.join("SKILL.md").display().to_string())
            );
            installed += 1;
        }
        Err(e) => eprintln!("swapdex: could not install the Codex skill: {e}"),
    }

    if installed == 0 {
        return Ok(1);
    }
    println!("  type `/swap` to pick an account, or `/swap <name>` to go straight there");
    println!("  (a plain `!swapdex use <account>` works too, without installing anything)");
    Ok(0)
}

/// `auto [on|off]` - read or set auto-continue: whether proxy mode may hand a
/// spent session to another account by itself. Kept as its own setting rather
/// than a flag you must remember, since the whole point is not having to think
/// about accounts.
pub fn auto(paths: &Paths, state: Option<&str>) -> Result<i32> {
    let s = crate::settings::load(paths);
    let Some(state) = state else {
        println!("auto-continue is {}", if s.auto() { "on" } else { "off" });
        return Ok(0);
    };
    let on = match state.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => true,
        "off" | "false" | "no" | "0" => false,
        other => {
            eprintln!("swapdex: expected `on` or `off`, got '{other}'");
            return Ok(2);
        }
    };

    crate::settings::update(paths, |c| c.proxy_auto = Some(on))?;
    println!(
        "auto-continue {}{}",
        if on { "on" } else { "off" },
        if on {
            " - a spent account hands the running session to another one"
        } else {
            " - the proxy stays on the account you chose"
        }
    );
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
                let rc = use_account(paths, &name, None, false, false)?;
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
    /// Read every account's usage. A free function so the dashboard can run it
    /// on a thread: doing it on the loop froze the screen for seconds.
    fn read_quota_usage(paths: &Paths) -> Vec<(String, crate::tui::Usage)> {
        let Ok(exe) = std::env::current_exe() else {
            return Vec::new();
        };
        let Ok(out) = Command::new(exe)
            .arg("quota")
            .arg("--json")
            .stdin(std::process::Stdio::null())
            .output()
        else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return Vec::new();
        };
        // Accept an array of account objects or {"accounts": [...]}.
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v.get("accounts").and_then(|a| a.as_array()).cloned())
            .unwrap_or_default();
        let mut claude: Vec<(String, crate::tui::Usage)> = arr
            .iter()
            .filter_map(|acc| {
                // `name` may carry an " (active)" marker; strip it to match Row.name.
                let name = acc
                    .get("name")?
                    .as_str()?
                    .trim_end_matches(" (active)")
                    .to_string();
                let win = |key: &str| -> (Option<f64>, Option<i64>) {
                    let w = acc.get(key);
                    (
                        w.and_then(|w| w.get("used_pct")).and_then(|v| v.as_f64()),
                        w.and_then(|w| w.get("resets_at")).and_then(|v| v.as_i64()),
                    )
                };
                let (five_h, five_h_reset) = win("five_hour");
                let (seven_d, seven_d_reset) = win("seven_day");
                // No numbers is still worth a row: empty tracks alone cannot
                // say whether the account was never asked, could not answer,
                // or has nothing left, and that ambiguity is exactly what
                // makes a healthy account look broken. Carry the reason.
                let note = match acc.get("status").and_then(|s| s.as_str()) {
                    Some("ok") | None => None,
                    Some("throttled") => Some("endpoint busy - retrying".to_string()),
                    Some("expired") => Some("login expired".to_string()),
                    Some("offline") => acc
                        .get("detail")
                        .and_then(|d| d.as_str())
                        // The long-form fix belongs in `swapdex quota`; the
                        // row has one column, so keep the first clause.
                        .map(|d| d.split(" - ").next().unwrap_or(d).to_string()),
                    Some(other) => Some(other.to_string()),
                };
                if five_h.is_none() && seven_d.is_none() && note.is_none() {
                    return None;
                }
                Some((
                    name,
                    crate::tui::Usage {
                        five_h,
                        five_h_reset,
                        seven_d,
                        seven_d_reset,
                        // A live read has no age to disclose.
                        observed_at: None,
                        note,
                        on_credits: acc
                            .get("on_credits")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        ident: None,
                    },
                ))
            })
            .collect();
        // Fill the gaps from what was read before. The usage endpoint
        // rate-limits when several accounts are asked in a row, and an account
        // that could not be read this minute is not an account with no quota -
        // blanking it looks exactly like a broken one. The reading is recorded
        // by `quota` itself, where it is taken; a run that got nothing has
        // nothing to record and must not overwrite what it failed to refresh.
        for (name, e) in crate::quota_cache::load(paths) {
            // An account read THIS refresh keeps its live numbers. One that
            // only carries a reason - busy, expired - takes the remembered
            // numbers instead: an old figure beats an empty track, as long as
            // it is shown with its age.
            if let Some((_, u)) = claude
                .iter_mut()
                .find(|(n, u)| *n == name && u.five_h.is_none() && u.seven_d.is_none())
            {
                u.five_h = e.five_h;
                u.five_h_reset = e.five_h_reset;
                u.seven_d = e.seven_d;
                u.seven_d_reset = e.seven_d_reset;
                u.observed_at = Some(e.at);
                // The age now carries the caveat. Keeping "endpoint busy"
                // beside numbers that are right there reads as a complaint
                // about figures the user can already see.
                u.note = None;
                continue;
            }
            if claude.iter().any(|(n, _)| *n == name) {
                continue;
            }
            claude.push((
                name,
                crate::tui::Usage {
                    five_h: e.five_h,
                    five_h_reset: e.five_h_reset,
                    seven_d: e.seven_d,
                    seven_d_reset: e.seven_d_reset,
                    // Shown with its age, so a remembered number is never
                    // mistaken for a live one.
                    observed_at: Some(e.at),
                    note: None,
                    on_credits: e.on_credits,
                    ident: None,
                },
            ));
        }
        // Codex usage comes from two places, and a row takes whichever answers.
        //
        // The account itself answers per CREDENTIAL and names itself. That is
        // the only source that can report a home holding no transcripts, which
        // is not a rare case - a saved account that has not been driven through
        // this machine has none, and its row was a permanent blank.
        //
        // Its transcripts answer for free and keep answering when the endpoint
        // is throttled or the machine is offline. A reading found there belongs
        // to the home it was read from; captioning it with whoever the switch
        // timeline said was PAYING moved real numbers onto the wrong row.
        //
        // This runs on a thread, off the render loop, which is what makes it
        // safe to put a network call here at all.
        let codex_homes: Vec<(String, std::path::PathBuf)> =
            crate::slots::Slots::open_for(paths, "codex")
                .map(|s| {
                    s.list()
                        .into_iter()
                        .map(|r| (r.name, r.config_dir))
                        .collect()
                })
                .unwrap_or_default();
        // What the proxy recorded while serving. Windows past their reset are
        // dropped on load, so a turned-over window never lingers here.
        let codex_seen = crate::quota_cache::load_for(paths, "codex");
        for (name, dir) in &codex_homes {
            let live = crate::proxy::codex::slot_auth(dir).and_then(|auth| {
                match crate::codex_usage::fetch(&auth) {
                    crate::codex_usage::Fetch::Ok(a) => Some(*a),
                    // Any other outcome falls through to the transcript rather
                    // than blanking the row: a throttled endpoint says nothing
                    // about the account behind it.
                    _ => None,
                }
            });
            let transcript = crate::codex_limits::for_slot(dir, now_secs(), 7 * 86_400);
            let seen_by_proxy = codex_seen.get(name).cloned();
            if let Some(row) = codex_row(
                name,
                live.as_ref(),
                codex_slot_email(dir).as_deref(),
                seen_by_proxy,
                transcript,
                now_secs() as i64,
            ) {
                claude.push(row);
            }
        }
        // The user asked for an account and the proxy is serving another. Say
        // so on the row they picked: resolving it silently leaves them looking
        // at an account they did not choose with no idea why.
        if let Some(why) = unhonoured_ask(
            crate::slots::Slots::open_for(paths, "claude-code")
                .ok()
                .and_then(|sl| {
                    sl.serving_dir()
                        .and_then(|d| sl.list().into_iter().find(|r| r.config_dir == d))
                        .map(|r| r.name)
                })
                .as_deref(),
            crate::proxy::serving_account_for(paths, "claude-code").as_deref(),
            proxy_acted_since_ask(paths, "claude-code"),
        ) {
            if let Some((_, u)) = claude.iter_mut().find(|(n, _)| why.contains(n.as_str())) {
                u.note = Some(why);
            }
        }
        claude
    }
    impl crate::tui::TuiCtx for Ctx<'_> {
        fn rows(&mut self) -> Vec<crate::tui::Row> {
            let Ok(store) = Store::open(self.paths) else {
                return Vec::new();
            };
            let active = active_by_tool(&store, self.paths);
            let cfg = crate::settings::load(self.paths);
            // A Vec, not a HashMap: the rows are rendered in this order, and a
            // HashMap iterates differently every run - the list visibly reshuffled
            // itself on each refresh, so a row could move out from under the
            // cursor between pressing a key and reading the result.
            let slot_dirs: Vec<(String, std::path::PathBuf)> =
                crate::slots::Slots::open(self.paths)
                    .map(|s| {
                        s.list()
                            .into_iter()
                            .map(|r| (r.name, r.config_dir))
                            .collect()
                    })
                    .unwrap_or_default();
            // Which account is actually taking turns. With a proxy running that is
            // the account SERVING them, not the one the pointer names: after a
            // rotation those differ, and marking a spent account "active" next to
            // the word "spent" is a contradiction the user has to decode.

            let active_claude = active_slot_name(self.paths, "claude-code");
            let active_codex = active_slot_name(self.paths, "codex");
            let slot_dir_of = |name: &str| {
                slot_dirs
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, d)| d.clone())
            };
            // Codex switches by pointer now too, so the same authority applies:
            // once a pointer exists it decides which Codex account is active. The
            // live login does not move when a pointer does, and consulting it
            // anyway marked two Codex accounts active at once.
            let codex_slots: Vec<(String, std::path::PathBuf)> =
                crate::slots::Slots::open_for(self.paths, "codex")
                    .map(|s| {
                        s.list()
                            .into_iter()
                            .map(|r| (r.name, r.config_dir))
                            .collect()
                    })
                    .unwrap_or_default();

            let list: Vec<crate::tui::Row> = store
                .list()
                .iter()
                .map(|p| {
                    let (email, tier, marker) = profile_summary(&store, &p.name, &p.tools);
                    let at: Vec<&str> = active
                        .iter()
                        .filter(|(_, n)| n == &p.name)
                        .map(|(t, _)| *t)
                        .collect();
                    // A profile that ALSO has a slot switches by pointer, so its
                    // active marker has to follow the pointer too - the live login
                    // does not move, and the marker would sit still after a switch.
                    // Exactly one Claude account is active, and there is an order
                    // of authority for which: the proxy actually serving turns,
                    // else the default pointer (the slot model's answer), else the
                    // live login. Consulting the live login WHILE a pointer exists
                    // marked two accounts active at once - the pointed-at one, and
                    // whatever happened to be signed into the bare config.
                    // Codex is a different tool and keeps its own answer.
                    let is_claude = p.tools.iter().any(|t| t == "claude-code");
                    let is_codex = p.tools.iter().any(|t| t == "codex");
                    // One resolver for every kind of row. This branch used to ask
                    // its own question - "is this the DEFAULT account?" - and so
                    // ignored the serving pointer entirely. An account that is
                    // both a saved profile and a slot draws as ONE row, and when
                    // the profile half won that merge, pressing Enter moved who
                    // pays and left the row reading "ready".
                    //
                    // `None` still means "nothing points anywhere", which is when
                    // the live login is the only answer there is.
                    let by_pointer = if is_claude {
                        active_claude.as_ref().map(|a| a == &p.name)
                    } else if is_codex {
                        active_codex.as_ref().map(|a| a == &p.name)
                    } else {
                        None
                    };
                    crate::tui::Row {
                        is_slot: slot_dir_of(&p.name).is_some(),
                        disabled: cfg.is_disabled(&p.name),
                        // A slot with no readable token cannot serve a turn; say so
                        // rather than letting it look ready and fail later.
                        //
                        // Asked the same way the slot rows below ask it. This one
                        // used `slot_token`, the wrapper that throws away WHY, so
                        // a Keychain that would not open read as an account never
                        // signed into - and a row said "no login" beside its own
                        // live usage figures. A profile and a slot sharing a name
                        // leaves only this row, so the lossy answer was the only
                        // one shown.
                        needs_login: row_needs_login(
                            "claude-code",
                            slot_dir_of(&p.name).as_deref(),
                        ),
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
                        active: by_pointer.unwrap_or(!at.is_empty()),
                        warn: marker,
                        also: Vec::new(),
                        stale: slot_dir_of(&p.name)
                            .is_some_and(|d| crate::proxy::creds::slot_token_expired(&d, now_ms())),
                    }
                })
                .collect::<Vec<_>>();
            // Slot accounts too. They are what proxy mode rotates between, so a
            // list without them manages everything except what actually serves the
            // turns. A slot counts as ACTIVE when the default pointer names it:
            // switching a slot moves that pointer and never touches a live login,
            // so judging it by the live login would leave the marker stuck where
            // it was.
            let mut list = list;
            // Codex accounts live in slots too now, and a dashboard that omits
            // them manages half the accounts while showing the other half's
            // superseded copy-model profiles beside them.
            for (name, dir) in &codex_slots {
                let r = crate::slots::SlotRecord {
                    name: name.clone(),
                    id: String::new(),
                    config_dir: dir.clone(),
                    adopted: false,
                    tool: "codex".into(),
                };
                list.push(crate::tui::Row {
                    is_slot: true,
                    disabled: cfg.is_disabled(&r.name),
                    needs_login: crate::proxy::codex::slot_auth(&r.config_dir).is_none(),
                    name: r.name.clone(),
                    // Codex records the signed-in address inside its id_token; the
                    // account id is what it authenticates with, and it is what
                    // distinguishes two rows when both are signed in.
                    ident: identity_column(codex_slot_email(&r.config_dir), None),
                    tools: "codex".into(),
                    active: active_codex.as_deref() == Some(r.name.as_str()),
                    warn: None,
                    also: Vec::new(),
                    stale: false,
                });
            }
            for (name, dir) in &slot_dirs {
                if list.iter().any(|r| &r.name == name) {
                    continue;
                }
                list.push(crate::tui::Row {
                    is_slot: true,
                    disabled: cfg.is_disabled(name),
                    // A Keychain that will not open is not an account that was
                    // never signed in; telling the user to log in again would
                    // send them to fix something that is not broken.
                    needs_login: row_needs_login("claude-code", Some(dir)),
                    name: name.clone(),
                    ident: identity_column(crate::proxy::creds::slot_email(dir), None),
                    tools: "claude-code".into(),
                    active: active_claude.as_deref() == Some(name.as_str()),
                    warn: None,
                    also: Vec::new(),
                    stale: crate::proxy::creds::slot_token_expired(dir, now_ms()),
                });
            }
            // One row per account (a snapshot and a slot for the same login are
            // one account), then grouped by tool so Claude and Codex read as two
            // sections rather than one mixed list.
            crate::tui::group_sorted(crate::tui::dedupe_by_identity(list))
        }
        fn switch(&mut self, name: &str) -> (bool, String) {
            self.pre_switch_first = crate::session_link::read_timeline(self.paths).is_empty();
            // Enter means "let this account serve me" - not "move where my
            // conversations live". Moving the store is what `use` does, and having
            // the most natural key do it split a history in two every time
            // somebody changed accounts, which is the opposite of the point.
            let tool = tool_of_account(self.paths, name);
            run_self(&["serve", name, "--tool", tool])
        }
        fn toggle_rotation(&mut self, name: &str) -> String {
            let mut cfg = crate::settings::load(self.paths);
            let paused = cfg.toggle_disabled(name);
            match crate::settings::save(self.paths, &cfg) {
                Ok(()) if paused => {
                    format!("{name} paused - the proxy will not pick it (Enter still switches)")
                }
                Ok(()) => format!("{name} back in rotation"),
                Err(e) => format!("could not save that: {e}"),
            }
        }
        fn delete(&mut self, name: &str) -> String {
            // Delegate to the command every other caller uses. Deleting here only
            // ever touched the STORE, and every account in this list is a slot -
            // so it answered "no profile named X" for exactly the accounts the
            // dashboard is made of. Renaming had the same fault and was fixed
            // without me looking at its neighbour.
            run_self(&["rm", name, "--yes"]).1
        }
        fn rename(&mut self, old: &str, new: &str) -> (bool, String) {
            // Delegate to the command every other caller uses. Renaming here only
            // ever touched the STORE, and every account in this list is a slot -
            // so renaming from the dashboard failed with "no profile named X" on
            // exactly the accounts the dashboard is made of.
            run_self(&["rename", old, new])
        }
        fn sign_in(&mut self, name: &str) -> (bool, String) {
            sign_in_child(self.paths, name, tool_of_account(self.paths, name))
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
        fn cached_quota(&mut self) -> Vec<(String, crate::tui::Usage)> {
            crate::quota_cache::load(self.paths)
                .into_iter()
                .map(|(name, e)| {
                    (
                        name,
                        crate::tui::Usage {
                            five_h: e.five_h,
                            five_h_reset: e.five_h_reset,
                            seven_d: e.seven_d,
                            seven_d_reset: e.seven_d_reset,
                            // Shown with its age: a remembered number that looked
                            // live would be worse than no number at all.
                            observed_at: Some(e.at),
                            note: None,
                            on_credits: e.on_credits,
                            ident: None,
                        },
                    )
                })
                .collect()
        }
        fn quota_pct(&mut self) -> Vec<(String, crate::tui::Usage)> {
            read_quota_usage(self.paths)
        }
        /// Start a reading and hand back the channel it arrives on. The read is
        /// several network round trips with backoff; on the loop it froze the
        /// dashboard with no keys and no cursor, which reads as a broken tool.
        fn quota_pct_async(
            &mut self,
        ) -> std::sync::mpsc::Receiver<Vec<(String, crate::tui::Usage)>> {
            let (tx, rx) = std::sync::mpsc::channel();
            let paths = self.paths.clone();
            std::thread::spawn(move || {
                let _ = tx.send(read_quota_usage(&paths));
            });
            rx
        }
        fn proxy_running(&mut self) -> bool {
            crate::proxy::running_port(self.paths).is_some()
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
        // Slots count as accounts. Checking only the store and the live logins
        // meant a slot-only install - which is every install now - could not open
        // its own dashboard until something happened to be signed in, and the
        // dashboard is where you go to sign in.
        let has_slots = crate::adapters::names().iter().any(|t| {
            crate::slots::Slots::open_for(paths, t)
                .map(|s| !s.list().is_empty())
                .unwrap_or(false)
        });
        if Store::open(paths)?.list().is_empty()
            && !has_slots
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
/// What a switch can actually reach for this tool, given whether a proxy carries
/// its traffic.
///
/// swapdex reported the Codex login, listed three Codex accounts, and never said
/// that nothing was carrying Codex traffic. A session opened believing swapdex
/// was "on" for it went straight to the vendor, and switching accounts changed
/// nothing it could see. Running without a proxy is a legitimate way to use
/// swapdex - so this is a fact, not a fault - but it is a fact only swapdex
/// knows, and it was keeping it.
fn serving_reach(has_proxy: bool, accounts: usize) -> Option<String> {
    if accounts == 0 {
        return None;
    }
    Some(if has_proxy {
        format!("{accounts} account(s), proxy carrying traffic - a switch moves a running session")
    } else {
        format!(
            "{accounts} account(s), no proxy - a switch takes effect in the NEXT session, \
             not one already running (`swapdex service install --tool <tool>` to change that)"
        )
    })
}

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

    // Permanent slots (the no-copy model): the slots, the default account the
    // claude shim follows, and whether the shim is installed.
    if let Ok(slots) = crate::slots::Slots::open(paths) {
        let list = slots.list();
        // Which swapdex the shims actually call. A shim embeds an ABSOLUTE
        // path to whichever swapdex wrote it, so with two copies installed -
        // npm and brew, say - updating one leaves the shims calling the
        // other. Nothing on screen says so: a fix ships, the user updates,
        // and the tool goes on running the old binary. That went unnoticed
        // for a full day here, and the check exists because of it.
        if let Ok(me) = std::env::current_exe() {
            let me = std::fs::canonicalize(&me).unwrap_or(me);
            let mut stale: Vec<(String, std::path::PathBuf)> = Vec::new();
            for tool in crate::adapters::names() {
                let f = crate::shim::shim_path_for(paths, tool);
                let Ok(text) = std::fs::read_to_string(&f) else {
                    continue;
                };
                let Some(called) = crate::shim::swapdex_path_in(&text) else {
                    continue;
                };
                let called = std::fs::canonicalize(&called).unwrap_or(called);
                if called != me {
                    stale.push((tool_binary(tool).to_string(), called));
                }
            }
            report(
                "shim target",
                stale.is_empty(),
                if stale.is_empty() {
                    format!("the shims call this swapdex ({})", me.display())
                } else {
                    format!(
                        "the {} shim calls a DIFFERENT swapdex ({}) - updating this one changes nothing it does; re-run `swapdex shim`",
                        stale[0].0,
                        stale[0].1.display()
                    )
                },
            );
        }

        // More than one real copy on PATH: one shadows the other, and
        // updating the shadowed one changes nothing anybody can see.
        let copies = crate::shim::swapdex_copies_on(&std::env::var("PATH").unwrap_or_default());
        report(
            "install",
            copies.len() < 2,
            match copies.len() {
                0 => "swapdex is not on PATH (running it by full path)".to_string(),
                1 => format!("one copy on PATH ({})", copies[0].display()),
                _ => format!(
                    "{} copies on PATH - `{}` wins and the rest are shadowed; keep one installer and remove the others",
                    copies.len(),
                    copies[0].display()
                ),
            },
        );
        // Whether this copy is the current one. The version check is the
        // only thing here that touches the network, and it is confined to
        // `doctor`: a check that ran on every command would be a request
        // nobody asked for. It exists because an install that silently did
        // NOTHING - a scope typo, a 404, no error the user kept - looks
        // exactly like an install that worked.
        let running = env!("CARGO_PKG_VERSION");
        match if paths.sandboxed() {
            None
        } else {
            crate::quota::latest_published()
        } {
            Some(latest) if crate::quota::is_behind(running, &latest) => report(
                "version",
                false,
                format!(
                    "running {running}, but {latest} is published - update, then check                          `swapdex --version` actually changed"
                ),
            ),
            Some(latest) => report("version", true, format!("{running} (latest is {latest})")),
            // Offline is not a fault: say what is known and move on.
            None => report(
                "version",
                true,
                format!("{running} - could not reach the registry to compare"),
            ),
        }

        if !list.is_empty() {
            report("slots", true, format!("{} account(s)", list.len()));
            match slots.default_dir() {
                Some(dir) => {
                    let name = list
                        .iter()
                        .find(|r| r.config_dir == dir)
                        .map(|r| r.name.as_str())
                        .unwrap_or("(unknown)");
                    report("default", true, format!("plain `claude` -> '{name}'"));
                }
                None => report(
                    "default",
                    true,
                    "no default account set - `swapdex use <name>`".into(),
                ),
            }
            // Conversations live inside the store they were started in, so
            // pointing `claude` at an account also decides which conversations
            // `-c` and `-r` can offer. When the default store holds work and is
            // not a registered account, that work becomes unreachable from a
            // plain `claude` with nothing to explain it - and swapdex is what
            // redirected it, so swapdex is what has to say so.
            {
                let bare = paths.claude_dir().to_path_buf();
                let registered = crate::slots::Slots::open_for(paths, "claude-code")
                    .map(|s| s.list().iter().any(|r| r.config_dir == bare))
                    .unwrap_or(false);
                let held = std::fs::read_dir(bare.join("projects"))
                    .map(|rd| rd.flatten().count())
                    .unwrap_or(0);
                if !registered && held > 0 {
                    report(
                        "default store",
                        false,
                        format!(
                            "~/.claude holds {held} project(s) of conversations but is not a \
                             swapdex account, so a plain `claude -r` cannot reach them while \
                             another account is active - register it to switch back: \
                             `swapdex adopt personal {}`",
                            crate::util::redact_path(&bare.display().to_string())
                        ),
                    );
                }
            }
            // A name above another account's numbers is worse than no name: it
            // is not "missing information", it is wrong information, and every
            // figure on that row belongs to somebody else.
            {
                let mixed: Vec<String> = crate::slots::Slots::open_for(paths, "claude-code")
                    .map(|s| s.list())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|r| {
                        crate::proxy::creds::identity_contradicts_login(&r.config_dir)
                            .map(|why| format!("'{}' is {why}", r.name))
                    })
                    .collect();
                // Stated, not counted as a problem: the evidence available here
                // cannot separate "two accounts got mixed" from "this person has
                // a personal plan and an organisation", and a problem count that
                // includes maybes is one people stop reading.
                for line in &mixed {
                    report("account identity", true, line.clone());
                }
            }
            // Two directories can hold ONE login. Nothing said so, and the
            // fleet then reads as more accounts than exist - while a rate limit
            // on one applies to its twin. Not a fault (keeping two directories
            // for one account is a fair thing to do), so it is stated rather
            // than counted as a problem.
            {
                let named: Vec<(String, Option<String>)> = crate::adapters::names()
                    .into_iter()
                    .flat_map(|t| {
                        crate::slots::Slots::open_for(paths, t)
                            .map(|s| s.list())
                            .unwrap_or_default()
                    })
                    .map(|r| {
                        (
                            r.name.clone(),
                            crate::proxy::creds::slot_account_uuid(&r.config_dir),
                        )
                    })
                    .collect();
                for group in crate::slots::slots_sharing_an_account(&named) {
                    report(
                        "shared account",
                        true,
                        format!(
                            "{} hold the same login in different directories - rotation \
                             counts them once, because a rate limit belongs to the account",
                            group.join(" and ")
                        ),
                    );
                }
            }
            // A slot named after a tool cannot be refused after the fact, but it
            // can be named: it reads as that tool's home and points elsewhere.
            {
                let colliding: Vec<String> = crate::slots::Slots::open_for(paths, "claude-code")
                    .map(|s| s.list())
                    .unwrap_or_default()
                    .into_iter()
                    .chain(
                        crate::slots::Slots::open_for(paths, "codex")
                            .map(|s| s.list())
                            .unwrap_or_default(),
                    )
                    .filter(|r| crate::slots::name_reads_as_a_tool_home(&r.name))
                    .map(|r| r.name)
                    .collect();
                if !colliding.is_empty() {
                    report(
                        "account names",
                        false,
                        format!(
                            "account '{}' reads as the tool's own home directory but points \
                             somewhere else - rename it so the two are not confused: \
                             `swapdex rename {} <name>` (the folder and login are untouched)",
                            colliding.join("', '"),
                            colliding[0]
                        ),
                    );
                }
            }
            // Shim ENGAGEMENT, not mere existence: an installed shim that PATH
            // never reaches looks set up while a plain `claude` still runs
            // bare - `swapdex use` flips the pointer and nothing reads it.
            let shim_file = crate::shim::shim_path(paths);
            let (shim_ok, shim_msg) = if !shim_file.exists() {
                (
                    true,
                    "claude shim not installed - run `swapdex shim` so a plain \
                     `claude` follows `swapdex use`"
                        .to_string(),
                )
            } else {
                let shim_dir_path = shim_file.parent().unwrap_or(&shim_file).to_path_buf();
                let shim_dir = shim_dir_path.display();
                let resolved = crate::shim::resolved_claude();
                let active = matches!(resolved, Some((_, true)));
                let profile = crate::shim::shell_profile_text();
                match crate::shim::shim_reach(
                    active,
                    profile.as_ref().map(|(_, t)| t.as_str()),
                    &shim_dir_path,
                ) {
                    crate::shim::ShimReach::Active => (
                        true,
                        "claude shim active - plain `claude` follows `swapdex use`".to_string(),
                    ),
                    // Set up correctly; THIS shell has not picked it up. Two
                    // ordinary causes - a shell that never reads the profile
                    // (a script, a cron job, `ssh host cmd`), or one that
                    // started before the profile was edited. Neither is a
                    // fault, and calling it one sends someone to fix a
                    // configuration that is already right.
                    crate::shim::ShimReach::ConfiguredElsewhere => {
                        let p = profile
                            .as_ref()
                            .map(|(p, _)| crate::util::redact_path(&p.display().to_string()))
                            .unwrap_or_else(|| "your shell profile".to_string());
                        (
                            true,
                            format!(
                                "claude shim set up in {p} but not on THIS shell's PATH - \
                                 open a new terminal (or `source {p}`); a shell that does \
                                 not read that file, like a script or `ssh host cmd`, \
                                 never will"
                            ),
                        )
                    }
                    crate::shim::ShimReach::Missing => (
                        false,
                        match resolved {
                            Some((found, _)) => format!(
                                "claude shim installed but NOT taking effect - plain `claude` \
                                 runs {} instead; add the shim first on PATH: \
                                 export PATH=\"{shim_dir}:$PATH\"",
                                found.display()
                            ),
                            None => format!(
                                "claude shim installed but PATH has no `claude` at all - \
                                 add it: export PATH=\"{shim_dir}:$PATH\""
                            ),
                        },
                    ),
                }
            };
            report("shim", shim_ok, shim_msg);

            // Per-slot login health (read-only). Flag only a slot with NO
            // login yet, or one whose token sat unrefreshed past STALE_DAYS
            // (by then the refresh token itself may be revoked). Routine
            // access-token expiry (hours) is NOT flagged - Claude silently
            // refreshes it on the next run (same no-spam rule as
            // profile_detail).
            use crate::adapters::claude::SlotLogin;
            for r in &list {
                let key = format!("slot:{}", r.name);
                match crate::adapters::claude::slot_login(&r.config_dir) {
                    SlotLogin::Absent => report(
                        &key,
                        true,
                        format!("no login yet - `swapdex run {}` once signs it in", r.name),
                    ),
                    SlotLogin::Present(Some(ts)) if now_ms() - ts > STALE_DAYS * 86_400_000 => {
                        let days = (now_ms() - ts) / 86_400_000;
                        report(
                            &key,
                            true,
                            format!(
                                "login idle ~{days}d - `swapdex run {}` once refreshes \
                                 it (re-login if it asks)",
                                r.name
                            ),
                        );
                    }
                    // Fresh, or present-but-undeterminable: stay quiet - doctor
                    // flags only what it can determine.
                    SlotLogin::Present(_) => {}
                }
            }
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

    // The proxy service. Its failure takes every session down at once, and this
    // check did not exist - so the outage that reads as "API error everywhere"
    // had nothing here to name it. The unit records an absolute path resolved at
    // install time; for an npm install that path carries the Node version, so
    // upgrading Node deletes it while the service still reads as installed.
    // A sandboxed run has no real service, and reading the machine's own unit
    // from a temporary store would report the developer's box, not the sandbox.
    let home = if paths.sandboxed() {
        None
    } else {
        dirs::home_dir()
    };
    for tool in ["claude-code", "codex"] {
        let Some(path) = home.as_ref().map(|h| {
            if cfg!(target_os = "macos") {
                crate::service::launchd_path(h, tool)
            } else {
                crate::service::systemd_path(h, tool)
            }
        }) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue; // not installed for this tool: not a problem
        };
        let label = format!("service:{}", crate::commands::tool_binary(tool));
        match crate::service::unit_program(&body) {
            Some(prog) if std::path::Path::new(prog).exists() => {
                let up = crate::proxy::running_proxy_for(paths, tool).is_some();
                if up {
                    report(&label, true, "installed and running".into());
                } else {
                    report(
                        &label,
                        false,
                        format!(
                            "installed but not running - `swapdex service restart --tool {}` \
                             (sessions fall back to your own login until it is up)",
                            tool
                        ),
                    );
                }
            }
            Some(prog) => report(
                &label,
                false,
                format!(
                    "points at {} which no longer exists - reinstall with \
                     `swapdex service install --tool {}` (an npm path carries the Node \
                     version, so upgrading Node breaks it)",
                    crate::util::redact_path(prog),
                    tool
                ),
            ),
            None => report(
                &label,
                false,
                format!("{} names no program - reinstall it", path.display()),
            ),
        }
    }

    // The pinned proxy address. `swapdex shim` refuses to write it unless a
    // proxy is alive, so pinning is safe - and then nothing watched it. When the
    // proxy went away, every session got "Connection refused" while the settings
    // still named an address nobody answered, and this health check said the
    // service was fine because it looked at the unit and the process, never at
    // the address. A pin nothing answers is a brick.
    {
        let settings_file = paths.claude_dir().join("settings.json");
        let pinned = std::fs::read_to_string(&settings_file)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| crate::shim::pinned_port(&v));
        if let Some(port) = pinned {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let answers =
                std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700))
                    .is_ok();
            if answers {
                report(
                    "proxy pin",
                    true,
                    format!("Claude Code -> 127.0.0.1:{port}, answering"),
                );
            } else {
                report(
                    "proxy pin",
                    false,
                    format!(
                        "settings.json sends Claude Code to 127.0.0.1:{port} and nothing is \
                         answering there - every session gets 'Connection refused'. Start the \
                         proxy (`swapdex service restart --tool claude-code`, from a terminal \
                         on this machine so it can read the Keychain), or remove the pin from \
                         {} to go direct.",
                        crate::util::redact_path(&settings_file.display().to_string())
                    ),
                );
            }
        }
    }

    // What a switch can actually reach, per tool. swapdex knew that nothing was
    // carrying a tool's traffic and never said so, so a session opened believing
    // swapdex was "on" for it was talking straight to the vendor.
    for tool in crate::adapters::names() {
        let accounts = crate::slots::Slots::open_for(paths, tool)
            .map(|s| s.list().len())
            .unwrap_or(0);
        let has_proxy = crate::proxy::running_proxy_for(paths, tool).is_some();
        if let Some(msg) = serving_reach(has_proxy, accounts) {
            report(&format!("reach:{}", tool_binary(tool)), true, msg);
        }
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

pub fn rm(paths: &Paths, name: &str, yes: bool, tool: Option<&str>) -> Result<i32> {
    if let Some(c) = reject_bad_name(name) {
        return Ok(c);
    }
    let store = Store::open(paths)?;
    // A slot account is not a saved snapshot: removing it means "stop managing
    // this account", and the directory holding its login is left untouched, so
    // the account itself is never lost.
    // Both tools keep accounts here. Looking only in Claude's registry meant a
    // Codex account could not be removed at all, and the dashboard - which lists
    // both - reported "no account named X" for something it was showing.
    let slot_tool = crate::adapters::names().into_iter().find(|t| {
        crate::slots::Slots::open_for(paths, t)
            .map(|s| s.get(name).is_some())
            .unwrap_or(false)
    });
    let is_slot = slot_tool.is_some();
    let is_profile = store.list().iter().any(|p| p.name == name);
    if !is_slot && !is_profile {
        eprintln!("swapdex: no account named '{name}'");
        return Ok(5);
    }
    // Naming a tool means "drop that tool", whatever else the name refers to.
    // The slot branch below used to run first, so on a name that was BOTH a
    // slot and a profile the flag was ignored and the whole account was
    // unregistered - the opposite of what was asked, and destructive.
    if let Some(t) = tool {
        // Both kinds: a tool can exist as a saved snapshot, as a registered
        // slot, or both. Looking only at snapshots answered "has no {t} login"
        // about a slot the listing was showing, and left no way to remove it
        // short of editing slots.json by hand.
        let dropped_snapshot = store.drop_tool(name, t)?;
        let dropped_slot = crate::slots::Slots::open_for(paths, t)
            .and_then(|mut sl| sl.remove(name))
            .unwrap_or(false);
        if !dropped_snapshot && !dropped_slot {
            eprintln!("swapdex: profile '{name}' has no {t} login");
            return Ok(5);
        }
        println!("dropped {t} from '{name}' (its live login keeps running, now unsaved)");
        return Ok(0);
    }
    if is_slot {
        if !yes {
            use std::io::IsTerminal;
            let tty =
                std::io::stdin().is_terminal() || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
            if !tty {
                eprintln!(
                    "swapdex: `rm {name}` unregisters that account (its login and folder \
                     stay). Re-run with --yes to confirm."
                );
                return Ok(7);
            }
            if !yes_no(
                &format!(
                    "stop managing account '{name}'? Its login and folder stay, so \
                     `swapdex adopt` can bring it back. [y/N]: "
                ),
                false,
            ) {
                println!("kept '{name}'.");
                return Ok(0);
            }
        }
        let mut slots = crate::slots::Slots::open_for(paths, slot_tool.unwrap_or("claude-code"))?;
        let dir = slots.get(name).map(|r| r.config_dir);
        slots.remove(name)?;
        println!("stopped managing '{name}'.");
        if let Some(d) = dir {
            println!(
                "  its login is untouched at {}",
                crate::util::redact_path(&d.display().to_string())
            );
        }
        // A profile of the same name is a separate thing; leave it alone.
        return Ok(0);
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
    // A slot account renames by mapping only: its directory (and so its Keychain
    // item, and so its login) must not move.
    //
    // Across every tool, not Claude's registry alone. Looking only there left a
    // Codex slot unfound: the snapshot was renamed and the slot kept its old
    // name, so one account answered to two - `ls` said one thing and the
    // registry another. `rm` already had this fix; rename did not.
    // An account can be BOTH a slot and a snapshot. Renaming the slot and
    // returning left the snapshot under the old name, so one account showed as
    // two rows - `after` and `before` side by side, each half of the same
    // login. Rename every kind that answers to this name.
    // EVERY tool's registry, not the first one `find_any_tool` happens to
    // name. Renaming one left the others behind, so an account holding both a
    // Claude and a Codex slot came out split in two - one login under two
    // names. 0.88.0 fixed the slot-vs-snapshot half of this; this is the
    // slot-vs-slot half.
    let mut slot_renamed = false;
    for tool in adapters::all().iter().map(|a| a.name()) {
        if let Ok(mut slots) = crate::slots::Slots::open_for(paths, tool) {
            match slots.rename(old, new) {
                Ok(true) => slot_renamed = true,
                Ok(false) => {}
                Err(e) => {
                    eprintln!("swapdex: {e}");
                    return Ok(6);
                }
            }
        }
    }
    // No snapshot to move as well: the slot rename was the whole job.
    if slot_renamed
        && Store::open(paths)
            .map(|st| !st.list().iter().any(|p| p.name == old))
            .unwrap_or(true)
    {
        println!("renamed account '{old}' to '{new}'");
        return Ok(0);
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
/// Repoint the default account at slot `name` (the `claude` shim follows this).
/// No credential is moved. Called by `use_account` when the name is a slot.
/// What a slot switch actually did, said plainly.
///
/// Moving the pointer is not the same as moving a conversation. Without a proxy
/// the change reaches the NEXT launch and nothing that is already running, and
/// reporting it as "this account now serves you" was simply false - the session
/// carried on with the old account while the line said otherwise.
pub fn switch_outcome_line(
    tool: &str,
    name: &str,
    proxy_running: bool,
    history_shared: bool,
) -> String {
    let bin = tool_binary(tool);
    if proxy_running {
        format!("{name} serves this session from the next turn ({bin} proxy is running)")
    } else {
        let flag = if tool == "codex" { " --tool codex" } else { "" };
        let mut out = format!("default {bin} account -> {name}\n");
        out.push_str(&format!(
            "  this applies to the NEXT {bin} you start; a session already open keeps the account it began with\n"
        ));
        out.push_str(&format!(
            "  to move one that is already running: swapdex proxy{flag}\n"
        ));
        // This used to warn that a switch changed which conversations `-c` and
        // `-r` could see, which was true and cost people time. Slots share their
        // history now, so it is no longer true - and a warning that has stopped
        // being true is worse than none, because it teaches a rule the tool no
        // longer follows. `share-history` repairs accounts made before that.
        if !history_shared {
            out.push_str(
                "  note: this account keeps its own conversation history - \
                 `swapdex share-history` makes them all reachable from every account",
            );
        }
        out
    }
}

fn use_slot_default(paths: &Paths, name: &str, tool: &str, dry_run: bool) -> Result<i32> {
    let bin = tool_binary(tool);
    if dry_run {
        println!("would set the default {bin} account -> {name}");
        return Ok(0);
    }
    let slots = crate::slots::Slots::open_for(paths, tool)?;
    slots.set_default(name)?;
    let proxy = crate::proxy::running_proxy_for(paths, tool).is_some();
    println!(
        "{}",
        switch_outcome_line(
            tool,
            name,
            proxy,
            crate::slots::history_is_shared(paths, tool)
        )
    );
    // First-time nudge: without the shim, a plain launch won't follow this.
    if !crate::shim::shim_path_for(paths, tool).exists() {
        println!(
            "  tip: run `swapdex shim` once so a plain `{bin}` follows your switches\n\
             \x20      (or launch directly with `swapdex run {name}`)"
        );
    }
    Ok(0)
}

/// Install the `claude` shim so a plain `claude` launches in the default
/// account's slot. Prints the one PATH line the user needs.
pub fn install_shim(paths: &Paths) -> Result<i32> {
    let (shim, shim_dir) = crate::shim::install(paths)?;
    println!("installed the claude shim at {}", shim.display());
    // Codex switches by pointer too, so a plain `codex` needs the same wrapper.
    // Not having Codex installed is not a failure - there is simply nothing to
    // wrap - so it is reported either way and never aborts the claude shim.
    // Reaching the proxy must not depend on winning the PATH. The shim only
    // fires when it does, and on a real machine another `claude` sat ahead of
    // it - so the proxy went unused and `serve` silently changed nothing for a
    // day, on two machines. Pinning the address in the tool's own settings is
    // what every competing proxy switcher does, and no PATH ordering undoes it.
    let svc = dirs::home_dir().is_some_and(|h| {
        if cfg!(target_os = "macos") {
            crate::service::launchd_path(&h, "claude-code").exists()
        } else {
            crate::service::systemd_path(&h, "claude-code").exists()
        }
    });
    match crate::shim::pin_base_url(paths, 8787, svc)? {
        Some(f) => println!(
            "  pinned the proxy address in {} - a plain `claude` reaches it \
             however it is started",
            crate::util::redact_path(&f.display().to_string())
        ),
        None => println!(
            "  not pinning the proxy address: no service keeps the proxy alive, and a \
             pinned address with nothing behind it would stop `claude` from starting.\n\
             \x20     run `swapdex service install --tool claude-code` first"
        ),
    }
    match crate::shim::install_codex(paths)? {
        Some(p) => println!("installed the codex shim at {}", p.display()),
        None => println!("  (no `codex` on PATH - skipped its shim)"),
    }
    // Put it on PATH ourselves. Leaving that to the user is how the shim ends up
    // installed but never reached: `swapdex use` then flips a pointer nothing
    // reads, and the switch appears to work while changing nothing.
    match crate::shim::ensure_on_path(&shim_dir)? {
        crate::shim::PathSetup::AlreadyThere => {
            // On the PATH is not the same as WINNING it. Reporting membership as
            // success is how the shim ends up installed and never reached: the
            // proxy is never used and `serve` silently does nothing, while the
            // install says everything is fine. Say which entry holds the name.
            let entries: Vec<String> = std::env::var("PATH")
                .unwrap_or_default()
                .split(':')
                .map(str::to_string)
                .collect();
            let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
            match crate::shim::path_verdict(&shim_dir, &refs) {
                crate::shim::PathVerdict::Wins => {
                    println!("  it is already on your PATH - a plain `claude` goes through it");
                }
                crate::shim::PathVerdict::Shadowed(winner) => {
                    println!(
                        "  it is on your PATH but {} comes first and holds `claude`, so the \
                         shim never runs - `swapdex serve` would change nothing.\n\
                         \x20     put the shim ahead of it:  export PATH=\"{}:$PATH\"",
                        crate::util::redact_path(&winner),
                        shim_dir.display()
                    );
                }
                crate::shim::PathVerdict::Absent => {
                    println!(
                        "  add this to your shell profile so it wins over the real claude:\n\
                         \x20     export PATH=\"{}:$PATH\"",
                        shim_dir.display()
                    );
                }
            }
        }
        crate::shim::PathSetup::Added(profile) => {
            println!(
                "  added it to {} - open a new terminal (or `source` that file) and a plain \
                 `claude` goes through it",
                crate::util::redact_path(&profile.display().to_string())
            );
        }
        crate::shim::PathSetup::Manual => {
            println!(
                "  add this to your shell profile so it wins over the real claude:\n\
                 \x20     export PATH=\"{}:$PATH\"",
                shim_dir.display()
            );
        }
    }
    Ok(0)
}

/// A [Y/n] prompt defaulting to yes. Non-interactive (no TTY) returns false so
/// onboarding never blocks a script - it prints guidance instead.
fn ask_yes(question: &str) -> bool {
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal() || std::env::var_os("SWAPDEX_ASSUME_TTY").is_some();
    if !tty {
        return false;
    }
    matches!(
        prompt(&format!("{question} [Y/n]"), "y").as_deref(),
        Some("y") | Some("Y") | Some("")
    )
}

/// The marker written once onboarding has been shown, so a bare `swapdex` does
/// not re-run the guided flow on every launch.
fn onboarded_marker(paths: &Paths) -> std::path::PathBuf {
    paths.store_dir().join("onboarded")
}

/// True when a bare `swapdex` should auto-run guided onboarding: it has not been
/// shown before, AND there is something to set up (existing `~/.claude-*` dirs to
/// register, or legacy copy-model Claude profiles to migrate). A brand-new user
/// with nothing to migrate is left to the normal banner/hints.
pub fn needs_onboarding(paths: &Paths) -> bool {
    if onboarded_marker(paths).exists() {
        return false;
    }
    let Ok(slots) = crate::slots::Slots::open(paths) else {
        return false;
    };
    let has_unregistered = paths
        .discover_claude_config_dirs()
        .iter()
        .any(|d| !slots.list().iter().any(|r| &r.config_dir == d));
    if has_unregistered {
        return true;
    }
    if let Ok(store) = Store::open(paths) {
        return store
            .list()
            .iter()
            .any(|p| p.tools.iter().any(|t| t == "claude-code") && slots.get(&p.name).is_none());
    }
    false
}

/// Guided first-run: detect what the user already has and walk them to a safe
/// slot setup, one [Y/n] at a time. Explains the win, hides the machinery.
pub fn onboard(paths: &Paths) -> Result<i32> {
    println!("swapdex gives each account its own space, so switching never logs you out.\n");

    // State 3: existing ~/.claude-* dirs the user runs by hand, not yet registered.
    let slots_now = crate::slots::Slots::open(paths)?;
    let unregistered: Vec<std::path::PathBuf> = paths
        .discover_claude_config_dirs()
        .into_iter()
        .filter(|d| !slots_now.list().iter().any(|r| &r.config_dir == d))
        .collect();
    if !unregistered.is_empty() {
        println!("Found Claude config dirs you already use:");
        for d in &unregistered {
            println!("  {}", d.display());
        }
        if ask_yes("Register them as swapdex accounts?") {
            let mut s = crate::slots::Slots::open(paths)?;
            for d in &unregistered {
                let name = d
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.trim_start_matches(".claude-").to_string())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "account".into());
                match s.adopt(&name, d) {
                    Ok(r) => println!("  registered '{}'", r.name),
                    Err(e) => eprintln!("  skipped {}: {e}", d.display()),
                }
            }
        }
        println!();
    }

    // State 2: legacy copy-model Claude profiles without a slot.
    if let Ok(store) = Store::open(paths) {
        let s = crate::slots::Slots::open(paths)?;
        let legacy = store
            .list()
            .into_iter()
            .filter(|p| p.tools.iter().any(|t| t == "claude-code") && s.get(&p.name).is_none())
            .count();
        if legacy > 0 {
            println!(
                "You have {legacy} saved Claude profile(s) on the old copy-switch model \
                 (the one that could log you out)."
            );
            if ask_yes("Give each its own space now?") {
                migrate(paths)?;
                println!();
            }
        }
    }

    // Casual convenience: make a plain `claude` follow `swapdex use`.
    if !crate::shim::shim_path(paths).exists()
        && ask_yes("Make a plain `claude` follow `swapdex use`? (installs a small shim)")
    {
        install_shim(paths)?;
        println!();
    }

    // Mark it shown so a bare `swapdex` does not re-run this every launch.
    let _ = std::fs::create_dir_all(paths.store_dir());
    let _ = std::fs::write(onboarded_marker(paths), b"1");

    // Wrap up.
    if crate::slots::Slots::open(paths)?.list().is_empty() {
        println!("No accounts yet. Log into Claude, then run: swapdex run <name>");
    } else {
        println!("You're set. `swapdex ui` shows your accounts and switches between them.");
    }
    Ok(0)
}

/// Create permanent slots for the legacy copy-model Claude profiles so they can
/// be used via `run`/`use` with no credential copying. Does NOT import a token
/// (a slot's login is created by a fresh sign-in - swapdex never writes a
/// credential); it prints the accounts to log into once.
pub fn migrate(paths: &Paths) -> Result<i32> {
    let store = Store::open(paths)?;
    let mut slots = crate::slots::Slots::open(paths)?;
    let mut created = Vec::new();
    for p in store.list() {
        if !p.tools.iter().any(|t| t == "claude-code") {
            continue;
        }
        if slots.get(&p.name).is_some() {
            continue;
        }
        // A profile named after the tool would become a slot named after the
        // tool, which reads as the tool's own home and is not. This is where
        // that collision was minted, so this is where it stops.
        let taken: Vec<String> = slots.list().into_iter().map(|r| r.name).collect();
        let name = if crate::slots::name_reads_as_a_tool_home(&p.name) {
            let safe = crate::slots::suggest_non_colliding(&p.name, &taken);
            println!(
                "  '{}' would read as the tool's own home, so the account is named '{safe}'",
                p.name
            );
            safe
        } else {
            p.name.clone()
        };
        if let Ok(rec) = slots.create(&name) {
            crate::slots::link_shared_config(&rec.config_dir, paths.claude_dir(), "claude-code");
            created.push(name);
        }
    }
    if created.is_empty() {
        println!("Nothing to migrate - every Claude account already has its own space.");
        return Ok(0);
    }
    println!(
        "Created slots for: {}. Each account now has its own space - the surprise\n\
         logouts when switching are gone.",
        created.join(", ")
    );
    println!("  Log into each once (creates its own login):");
    for n in &created {
        println!("    swapdex run {n}");
    }
    if !crate::shim::shim_path(paths).exists() {
        println!("  Then `swapdex shim` so a plain `claude` follows `swapdex use`.");
    }
    Ok(0)
}

/// Share MCP servers across slots. Each Claude slot keeps its own `.claude.json`
/// (which mixes the per-account `oauthAccount` with the shareable `mcpServers`),
/// so MCP config cannot simply be symlinked like `settings.json`. This copies the
/// `mcpServers` block from the bare `~/.claude.json` into every slot's own
/// `.claude.json`, preserving each slot's account identity and other keys. Run it
/// after logging into your slots (a slot has no `.claude.json` until first login).
pub fn sync_mcp(paths: &Paths) -> Result<i32> {
    let src_path = paths.claude_config_json();
    let src: Value = if src_path.exists() {
        serde_json::from_slice(&crate::atomic::read_regular(&src_path)?).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let mcp = src
        .get("mcpServers")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let n = mcp.as_object().map(|o| o.len()).unwrap_or(0);
    if n == 0 {
        println!("No MCP servers in ~/.claude.json to share.");
        return Ok(0);
    }
    let slots = crate::slots::Slots::open(paths)?;
    let mut synced = 0;
    let mut pending = 0;
    for r in slots.list() {
        let target = r.config_dir.join(".claude.json");
        if !target.exists() {
            pending += 1;
            continue;
        }
        let mut cfg: Value =
            serde_json::from_slice(&crate::atomic::read_regular(&target)?).unwrap_or(Value::Null);
        let Some(obj) = cfg.as_object_mut() else {
            continue;
        };
        obj.insert("mcpServers".into(), mcp.clone());
        crate::atomic::write_secret(&target, &serde_json::to_vec(&cfg)?)?;
        synced += 1;
    }
    println!("shared {n} MCP server(s) into {synced} account(s).");
    if pending > 0 {
        println!(
            "  {pending} account(s) have no login yet - sign them in from `swapdex ui` (the `l` key)."
        );
    }
    Ok(0)
}

/// Register an existing `CLAUDE_CONFIG_DIR` directory as a slot, in place.
pub fn adopt_slot(
    paths: &Paths,
    name: &str,
    dir: &std::path::Path,
    sel: Option<ToolSel>,
) -> Result<i32> {
    let tool = slot_tool(sel);
    let mut slots = crate::slots::Slots::open_for(paths, tool)?;
    let rec = slots.adopt(name, dir)?;
    println!(
        "registered '{}' ({tool}) -> {}",
        rec.name,
        rec.config_dir.display()
    );
    Ok(0)
}

/// Launch Claude in `<name>`'s permanent slot (create the slot on first use).
/// swapdex never writes the credential here - the tool's own login does, into
/// the slot's own `CLAUDE_CONFIG_DIR`. `exec` replaces this process, so this
/// only returns on failure.
pub fn run_account(
    paths: &Paths,
    name: &str,
    sel: Option<ToolSel>,
    no_launch: bool,
    args: &[String],
) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let tool = slot_tool(sel);
    let Some(home_var) = crate::slots::home_var(tool) else {
        eprintln!(
            "swapdex: {tool} has no per-account home to launch into - only claude and codex do"
        );
        return Ok(2);
    };
    let mut slots = crate::slots::Slots::open_for(paths, tool)?;
    let rec = match slots.get(name) {
        Some(r) => r,
        None => {
            let r = slots.create(name)?;
            crate::slots::link_shared_config(&r.config_dir, &shared_source(paths, tool), tool);
            r
        }
    };
    if no_launch {
        println!("account '{name}' is ready ({tool})");
        println!(
            "  its home: {}",
            crate::util::redact_path(&rec.config_dir.display().to_string())
        );
        return Ok(0);
    }
    let bin = tool_binary(tool);
    if !command_exists(bin) {
        eprintln!("swapdex: `{bin}` isn't on your PATH. Install it, then retry.");
        return Ok(3);
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args).env(home_var, &rec.config_dir);
    // This is the path a sign-in takes, and signing in must reach the vendor
    // directly. An inherited proxy address both breaks the OAuth code exchange
    // and answers with whichever account the proxy already holds - so a fresh
    // slot appears to be signed in as somebody else, or its prompt takes no
    // input at all. A slot launched here serves itself, so the proxy has nothing
    // to add either way.
    for var in ["ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY"] {
        cmd.env_remove(var);
    }
    let err = cmd.exec();
    Err(anyhow::anyhow!("failed to launch {bin}: {err}"))
}

/// Sign an account in and come BACK.
///
/// `run_account` replaces this process, which is right for `swapdex run` from a
/// shell and wrong for the dashboard: signing in one account tore the dashboard
/// down, so adding several meant relaunching it between each. Here the tool is a
/// child process that owns the terminal while it runs, and when it exits the
/// caller is still alive to redraw.
pub(crate) fn sign_in_child(paths: &Paths, name: &str, tool: &str) -> (bool, String) {
    let Some(home_var) = crate::slots::home_var(tool) else {
        return (
            false,
            format!("{tool} has no per-account home to sign into"),
        );
    };
    let mut slots = match crate::slots::Slots::open_for(paths, tool) {
        Ok(s) => s,
        Err(e) => return (false, format!("cannot open the account list: {e}")),
    };
    let rec = match slots.get(name) {
        Some(r) => r,
        None => match slots.create(name) {
            Ok(r) => {
                crate::slots::link_shared_config(&r.config_dir, &shared_source(paths, tool), tool);
                r
            }
            Err(e) => return (false, format!("could not make a space for '{name}': {e}")),
        },
    };
    let bin = tool_binary(tool);
    if !command_exists(bin) {
        return (false, format!("`{bin}` isn't on your PATH"));
    }
    // Signing in must reach the vendor directly, and there are two ways it did
    // not. An inherited proxy ADDRESS answers with whichever account the proxy
    // already holds - that one is cleared below. And the shim itself puts the
    // proxy in front of any Codex run it does not recognise as plain: a bare
    // launch is not recognised, so pressing the sign-in key opened a session
    // served by the account that was already paying. An account with no login
    // came up looking signed in, and nothing about it was true. So: the REAL
    // binary, never the shim, and the subcommand that actually signs in.
    let exe = crate::shim::real_tool(paths, tool).unwrap_or_else(|| std::path::PathBuf::from(bin));
    match spawn_tool_login_in(&exe, tool, Some((home_var, rec.config_dir.as_path()))) {
        Ok(_) => {
            // Whether the sign-in succeeded is the credential's story, not the
            // exit code's: the tool exits 0 when the user simply quits it.
            let signed_in = match tool {
                "codex" => crate::proxy::codex::slot_auth(&rec.config_dir).is_some(),
                _ => crate::proxy::creds::slot_token(&rec.config_dir).is_some(),
            };
            if signed_in {
                (true, format!("'{name}' is signed in"))
            } else {
                (
                    false,
                    format!("'{name}' still has no login - run it again and complete the sign-in"),
                )
            }
        }
        Err(e) => (false, format!("could not start {bin}: {e}")),
    }
}

/// Which tool an account belongs to.
///
/// This was worked out separately at every place that needed it - signing in,
/// switching, renaming, removing - and the versions disagreed. One of them fell
/// back to Claude whenever the slot registry did not know the name, so opening
/// the login for a Codex account launched Claude's.
///
/// A saved profile states its tool outright, so it is asked first; a slot is
/// registered under the tool it was made for; and only with neither is Claude
/// assumed, because that is what an unqualified account was before Codex had
/// accounts at all.
pub(crate) fn tool_of_account(paths: &Paths, name: &str) -> &'static str {
    let tools = crate::adapters::names();
    if let Some(t) = Store::open(paths).ok().and_then(|st| {
        st.list()
            .into_iter()
            .find(|p| p.name == name)
            .and_then(|p| {
                tools
                    .iter()
                    .copied()
                    .find(|t| p.tools.iter().any(|x| x == t))
            })
    }) {
        return t;
    }
    tools
        .iter()
        .copied()
        .find(|t| {
            crate::slots::Slots::open_for(paths, t)
                .map(|s| s.get(name).is_some())
                .unwrap_or(false)
        })
        .unwrap_or("claude-code")
}

/// The tool a slot command means. Slots exist for the two tools that can be
/// pointed at a per-account home; anything else is the caller's error to report.
pub(crate) fn slot_tool(sel: Option<ToolSel>) -> &'static str {
    match sel {
        Some(ToolSel::Codex) => "codex",
        Some(ToolSel::Gemini) => "gemini",
        Some(ToolSel::Antigravity) => "antigravity",
        // Claude is the default: it is what slots were built for, and every
        // existing `swapdex run <name>` must keep meaning the same thing.
        _ => "claude-code",
    }
}

pub(crate) fn tool_binary(tool: &str) -> &'static str {
    match tool {
        "codex" => "codex",
        _ => "claude",
    }
}

/// The bare config dir a new slot borrows its shared, account-agnostic files
/// from.
fn shared_source(paths: &Paths, tool: &str) -> std::path::PathBuf {
    match tool {
        "codex" => paths.codex_dir().to_path_buf(),
        _ => paths.claude_dir().to_path_buf(),
    }
}

/// `swapdex whereis [project]` - which account's store holds a conversation.
///
/// Switching accounts also switches which conversations `claude -c` and
/// `claude -r` can see, because Claude keeps them inside the config dir it was
/// launched with. Nothing is lost, but "no conversation found" reads exactly
/// like it was, so this says where the conversation actually is and how to
/// reopen it from any account.
pub fn whereis(paths: &Paths, project: Option<&str>) -> Result<i32> {
    // Both tools: a conversation is lost the same way in either, and someone
    // looking for theirs does not know which store to ask about.
    let mut found = crate::whereis::find(paths, project, 15);
    found.extend(crate::whereis::find_codex(paths, project, 15));
    found.sort_by_key(|f| std::cmp::Reverse(f.modified));
    found.truncate(15);
    if found.is_empty() {
        match project {
            Some(p) => println!("No conversation under a path matching '{p}', in any account."),
            None => println!("No conversations found in any account's store yet."),
        }
        return Ok(0);
    }
    println!("conversations, newest first - the account column is whose store holds it\n");
    let width = found
        .iter()
        .map(|f| f.account.chars().count())
        .max()
        .unwrap_or(8);
    for f in &found {
        println!(
            "  {:<width$} {:>8}  {}",
            f.account,
            // age_line takes the moment itself, not how long ago it was.
            age_line(f.modified as u128 * 1_000_000_000),
            f.project,
        );
        println!("    {}", f.resume_command());
    }
    println!(
        "\nnaming the config dir is what makes these work from any account - the shim\n\
         only fills that variable in when it is unset, so an explicit one always wins."
    );
    Ok(0)
}

/// `swapdex resume [project]` - reopen a conversation without first working out
/// which account owns it.
///
/// The stores cannot be merged: Claude writes a conversation into whichever
/// config dir it was launched with, and that separation is the same property
/// that stopped accounts logging each other out. What CAN be merged is the
/// looking - so this searches every account, picks the newest match, and
/// launches Claude against the store that actually holds it.
pub fn resume(paths: &Paths, project: Option<&str>) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    // No argument means "this project", which is the common case and saves the
    // user having to spell out a path they are already standing in.
    let cwd = std::env::current_dir().ok();
    let filter = project
        .map(str::to_string)
        .or_else(|| cwd.as_ref().map(|d| d.display().to_string()));
    let found = crate::whereis::find(paths, filter.as_deref(), 5);
    let Some(top) = found.first() else {
        match project {
            Some(p) => {
                eprintln!("swapdex: no conversation under a path matching '{p}', in any account")
            }
            None => eprintln!(
                "swapdex: no conversation for this directory in any account - \
                 `swapdex whereis` lists what there is"
            ),
        }
        return Ok(5);
    };
    println!(
        "resuming in '{}' ({})",
        top.account,
        crate::util::redact_path(&top.config_dir.display().to_string())
    );
    if found.len() > 1 {
        // Say that a choice was made, and how to make a different one.
        println!(
            "  (newest of {} here - `swapdex whereis` lists the rest)",
            found.len()
        );
    }
    if !command_exists("claude") {
        eprintln!("swapdex: `claude` isn't on your PATH. Install it, then retry.");
        return Ok(3);
    }
    // Naming the store explicitly is what makes this work from any account: the
    // shim only fills that variable in when it is unset.
    let err = std::process::Command::new("claude")
        .arg("-r")
        .arg(&top.session_id)
        .env("CLAUDE_CONFIG_DIR", &top.config_dir)
        .exec();
    Err(anyhow::anyhow!("failed to launch claude: {err}"))
}

/// `swapdex serve [name]` - hand turns to an account without moving where new
/// sessions start.
///
/// This is the operation the whole tool exists for: one place where all your
/// conversations live, and accounts swapped underneath it as they run out. It is
/// what a credential-copying switcher gets for free by only ever having one
/// store - and what swapdex had to separate deliberately, because isolating
/// accounts is also what isolates their conversations.
/// Is there anything for the dashboard to show?
///
/// A bare `swapdex` opens the picker when the answer is yes. It used to ask only
/// about saved profiles and live logins, never about slots - which is what
/// `run`, `adopt`, and `onboard` create. So the model swapdex steers people into
/// did not count as having accounts, and a user whose accounts were all slots
/// got a banner where the picker should have been.
pub fn has_any_account(paths: &Paths) -> bool {
    if Store::open(paths)
        .map(|st| !st.list().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    crate::adapters::names().into_iter().any(|t| {
        crate::slots::Slots::open_for(paths, t)
            .map(|s| !s.list().is_empty())
            .unwrap_or(false)
    })
}

/// Which account a dashboard row should be marked active for.
///
/// A running proxy's own record wins, because after a rotation it may be
/// serving someone other than the one chosen; otherwise the pointers decide.
/// Claude's rows already asked this way and Codex's asked only its pointer, so
/// handing turns to a Codex account left the mark where it was and the change
/// read as nothing having happened. One resolution for both tools.
pub fn active_slot_name(paths: &Paths, tool: &str) -> Option<String> {
    let slots = crate::slots::Slots::open_for(paths, tool).ok()?;
    let name_of = |dir: std::path::PathBuf| {
        slots
            .list()
            .into_iter()
            .find(|r| r.config_dir == dir)
            .map(|r| r.name)
    };
    pick_active(
        slots.serving_dir().and_then(&name_of),
        crate::proxy::serving_account_for(paths, tool),
        slots.default_dir().and_then(&name_of),
        proxy_acted_since_ask(paths, tool),
    )
}

/// Has the proxy served a turn SINCE the account was chosen?
///
/// Both records are files, and their timestamps answer it: the ask is written
/// when the user picks, the proxy's record when it forwards. If the proxy's is
/// the newer of the two, it has had its say about the choice.
///
/// False when either is missing - no ask cannot be thwarted, and a proxy that
/// has never served has not contradicted anything.
fn proxy_acted_since_ask(paths: &Paths, tool: &str) -> bool {
    let asked = crate::slots::Slots::open_for(paths, tool)
        .ok()
        .map(|s| s.serving_pointer_file());
    let did = crate::proxy::serving_record_file(paths, tool);
    let stamp = |p: Option<std::path::PathBuf>| {
        p.and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
    };
    match (stamp(asked), stamp(Some(did))) {
        (Some(a), Some(d)) => d > a,
        _ => false,
    }
}

/// The order of authority behind that mark: what was ASKED FOR, then what
/// HAPPENED, then where sessions start.
///
/// It used to read the other way round, and the past outranked the instruction:
/// `proxy-serving` is what the proxy LAST did, which until the next turn goes
/// out is still the previous account. So pressing Enter changed who pays and the
/// row went on naming the old one, with nothing to say the key had worked.
///
/// A rotation still shows, because it happens when nobody asked for anything -
/// exactly when the proxy's own record is the only answer there is.
pub fn pick_active(
    asked_for: Option<String>,
    proxy_did: Option<String>,
    default: Option<String>,
    proxy_acted_since_ask: bool,
) -> Option<String> {
    // The ask wins until the proxy has served someone ELSE since it was made.
    // At that point the ask demonstrably did not take, and going on naming it
    // tells the user their key worked when it did not - seen on a real machine,
    // where an account asked for at 13:45 was still marked active half an hour
    // later while every turn went elsewhere because it refuses on overage.
    if proxy_acted_since_ask {
        if let (Some(a), Some(d)) = (asked_for.as_deref(), proxy_did.as_deref()) {
            if a != d {
                return proxy_did;
            }
        }
    }
    asked_for.or(proxy_did).or(default)
}

/// An ask the proxy is not honouring, in words - or `None` when there is none.
///
/// Resolving the disagreement silently would leave the user staring at an
/// account they did not choose with no idea why. They asked for something; the
/// screen owes them the reason it is not happening.
pub fn unhonoured_ask(
    asked_for: Option<&str>,
    proxy_did: Option<&str>,
    proxy_acted_since_ask: bool,
) -> Option<String> {
    if !proxy_acted_since_ask {
        return None;
    }
    match (asked_for, proxy_did) {
        (Some(a), Some(d)) if a != d => Some(format!(
            "asked for {a} - it cannot serve, so turns are going to {d}"
        )),
        _ => None,
    }
}

/// What a screen should call the account paying the next turn.
///
/// Codex prints this on /status, and it is the only identity it prints. A name
/// alone would claim an account is paying even when it has no login to pay
/// with, which is the case the proxy handles by quietly forwarding the client's
/// own credential instead. So the reason travels with the name.
pub fn payer_label(paths: &Paths, tool: &str) -> Option<String> {
    let slots = crate::slots::Slots::open_for(paths, tool).ok()?;
    let who = slots.payer()?;
    let rec = slots.get(&who)?;
    let email = match tool {
        "codex" => codex_slot_email(&rec.config_dir),
        _ => crate::proxy::creds::slot_email(&rec.config_dir),
    };
    // The DEFAULT pointer, not `active_slot_name` - that one consults the
    // serving pointer first, so it always agreed with the payer and the note
    // could never appear. `use` is what decides where a plain launch lands.
    let home = slots.default_dir().and_then(|d| {
        slots
            .list()
            .into_iter()
            .find(|r| r.config_dir == d)
            .map(|r| r.name)
    });
    Some(format!(
        "{}{}",
        payer_line(
            &who,
            email.as_deref(),
            crate::proxy::has_login(tool, &rec.config_dir),
        ),
        home_note(&who, home.as_deref())
    ))
}

/// Where the session's files live, when that is NOT the account paying for it.
///
/// swapdex keeps two pointers on purpose: `serve` decides who PAYS, `use`
/// decides where new sessions LIVE. Codex shows one field, so it showed the
/// payer - and a session billed to `work` while its history piled up in
/// `codex-main` looked, from that one line, like it was running as `work`.
/// Naming the home too costs a few characters and only when they differ; when
/// they agree there is nothing to disambiguate.
pub fn home_note(payer: &str, home: Option<&str>) -> String {
    match home {
        Some(h) if h != payer => format!(" - home: {h}"),
        _ => String::new(),
    }
}

/// The one line Codex has room for. Its `/status` prints the provider name and
/// nothing else about identity, so this is where the account has to appear -
/// and a SLOT NAME is not an account. `work` is a label its owner chose; it
/// does not say which login is being billed, which is the question somebody
/// reads that line to answer.
pub fn payer_line(name: &str, email: Option<&str>, has_login: bool) -> String {
    match (email.filter(|e| *e != name), has_login) {
        (Some(e), true) => format!("{name} ({e})"),
        (Some(e), false) => format!("{name} ({e}, no login)"),
        (None, true) => name.to_string(),
        // The shape callers already depended on, kept: a name with nothing
        // known about it still says plainly that it cannot pay.
        (None, false) => format!("{name} (no login)"),
    }
}

/// `--tool codex` where it is needed, nothing where it is not.
fn tool_flag(tool: &str) -> &'static str {
    if tool == "codex" {
        " --tool codex"
    } else {
        ""
    }
}

pub fn serve(
    paths: &Paths,
    name: Option<&str>,
    off: bool,
    sel: Option<ToolSel>,
    quiet: bool,
) -> Result<i32> {
    let tool = slot_tool(sel);
    let bin = tool_binary(tool);
    let slots = crate::slots::Slots::open_for(paths, tool)?;
    // A pointer naming an account that is gone is inert but not harmless: adopt
    // its directory back and it would start paying again. This is the command
    // that owns the pointer, so it is the one that clears it.
    slots.prune_serving();
    // The bare answer, for a caller that puts it somewhere else - the codex shim
    // labels its provider with it. The question there is "who pays", so it takes
    // the default when nobody is directing turns; naming only the explicit case
    // would leave the common one anonymous. Silence means there is no account at
    // all, which is a real answer and not a failure, so it still exits 0.
    if quiet {
        if let Some(label) = payer_label(paths, tool) {
            // Append what that account has left, from cache. Going through the
            // proxy costs the tool its own rate_limits block, so a status line
            // reading those prints "N/A" while swapdex holds a reading from a
            // minute ago. No request is made: a bar redraws constantly.
            let who = label
                .split_whitespace()
                .next()
                .unwrap_or(&label)
                .to_string();
            let cache = crate::quota_cache::load_for(paths, tool);
            let brief = cache
                .get(&who)
                .map(|e| {
                    // Carry the number's age. Without it the bar showed a
                    // reading taken hours earlier exactly like one taken now.
                    let refresh = crate::proxy::pick::measure_after(crate::proxy::pick::headroom(
                        e.five_h, e.seven_d,
                    ))
                    .as_secs() as i64;
                    let age = bar_age((now_secs() as i64).saturating_sub(e.at), refresh)
                        .unwrap_or_default();
                    format!("{}{age}", quota_brief(e.five_h, e.seven_d))
                })
                // No entry at all is the same news as an entry with no numbers.
                .unwrap_or_else(|| quota_brief(None, None));
            if brief.is_empty() {
                print!("{label}");
            } else {
                print!("{label} - {brief}");
            }
        }
        return Ok(0);
    }
    if off {
        slots.clear_serving()?;
        println!("each session pays for itself again ({bin})");
        return Ok(0);
    }
    let Some(name) = name else {
        match slots.serving_dir() {
            Some(dir) => {
                let who = slots
                    .list()
                    .into_iter()
                    .find(|r| r.config_dir == dir)
                    .map(|r| r.name)
                    .unwrap_or_else(|| "(unknown)".into());
                println!("turns are served by '{who}' ({bin})");
            }
            None => println!(
                "no account is directing turns ({bin}) - each session pays for itself\n                   `swapdex serve <name>` hands them to one without moving your conversations"
            ),
        }
        return Ok(0);
    };
    let Some(rec) = slots.get(name) else {
        // Name the accounts here rather than sending the reader to another
        // screen to read four words - and name BOTH kinds. Listing only slots
        // answered "no accounts saved yet" about a profile `add` had just
        // saved and `ls` was already showing.
        let mut servable: Vec<String> = crate::slots::Slots::open_for(paths, tool)
            .map(|sl| sl.list().into_iter().map(|r| r.name).collect())
            .unwrap_or_default();
        servable.sort();
        let mut saved: Vec<String> = Store::open(paths)
            .map(|st| st.list().into_iter().map(|p| p.name).collect())
            .unwrap_or_default();
        saved.sort();
        let sv: Vec<&str> = servable.iter().map(String::as_str).collect();
        let sa: Vec<&str> = saved.iter().map(String::as_str).collect();
        eprintln!(
            "swapdex: {}",
            unknown_account_or_unservable(name, &sv, &sa, tool)
        );
        return Ok(5);
    };
    // Handing turns to an account with no login does not fail loudly: the proxy
    // steps aside and forwards the client's OWN credential, so the turn works
    // and somebody else pays for it while every screen names this account. Refuse
    // the state rather than report it after the fact.
    if !crate::proxy::has_login(tool, &rec.config_dir) {
        eprintln!(
            "swapdex: '{name}' has no {bin} login, so it cannot pay for turns - your own account would, while every screen named '{name}'"
        );
        eprintln!(
            "  sign it in first: `swapdex run {name}{}`",
            tool_flag(tool)
        );
        return Ok(6);
    }
    slots.set_serving(name)?;
    // Record WHO PAYS from here on. Only `use` and `restore` were written, so the
    // action that changes the payer left no trace - and Codex usage, which comes
    // out of a transcript written in whichever home was running, has no other way
    // to know whose token produced those numbers.
    if let Ok(store) = Store::open(paths) {
        let _ = store.append_timeline(tool, name, crate::session_link::SERVE);
    }
    // Start the proxy this needs. Directing turns with nothing to carry them is
    // a setting that quietly does nothing, and telling someone to run a second
    // command to make the first one take effect is the same as not doing it.
    // Starting one is a convenience, and the proxy is DETACHED so it outlives
    // the shell that asked. Under a sandboxed root that daemon outlives the
    // temporary store itself and keeps the port, answering for a directory that
    // is deleted moments later - one was found still bound hours after. An
    // explicit `swapdex proxy` there is still honoured; this implicit one is not.
    if !paths.sandboxed() && crate::proxy::running_proxy_for(paths, tool).is_none() {
        let _ = proxy_ensure(paths, DEFAULT_PROXY_PORT, tool);
    }
    let live = crate::proxy::running_proxy_for(paths, tool).is_some();
    // Confirm the switch here rather than making the user run `ls` to see
    // whether it took and `usage` to see whether that account has room. What is
    // unknown stays unsaid - a window nobody has read gets no percentage.
    {
        let email = crate::slots::Slots::open_for(paths, tool)
            .ok()
            .and_then(|sl| sl.get(name).map(|r| r.config_dir.clone()))
            .and_then(|d| crate::proxy::creds::slot_email(&d));
        let left = crate::quota_cache::load_for(paths, tool)
            .get(name)
            .and_then(|e| e.seven_d)
            .map(|used| (100.0 - used).clamp(0.0, 100.0));
        println!("{}", switch_line(name, email.as_deref(), left));
    }
    if live {
        println!("  the session you have open moves from its next turn");
        // Codex reads its provider label once, at launch. The turn is billed to
        // the new account immediately, but a window already open keeps printing
        // the old name on /status - say so rather than let the screen argue with
        // the truth.
        if tool == "codex" {
            println!("  a codex window already open still shows the old name on /status");
        }
    } else {
        println!(
            "  but nothing is carrying them yet - `swapdex proxy{}` in another terminal",
            if tool == "codex" { " --tool codex" } else { "" }
        );
    }
    println!(
        "  your conversations stay where they are - this changed who pays, not where you work"
    );
    Ok(0)
}

/// A Codex row's identity label, and a warning when the two answers disagree.
///
/// Two things claim to name the account. The usage endpoint says whose token
/// the server just accepted; the home's saved `id_token` says whose the home
/// believes it is. They are normally the same, and when they are not, the live
/// answer is the true one and the disagreement is worth saying out loud: that
/// gap is the shape of the identity mix-up where signing in as one account
/// leaves another connected.
///
/// With no live answer the saved label stands unqualified. It is all there is,
/// and marking it suspect would be inventing a doubt rather than reporting one.
pub fn codex_identity(
    live_email: Option<&str>,
    live_plan: Option<&str>,
    saved_email: Option<&str>,
) -> String {
    fn clean(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    let (live, saved) = (clean(live_email), clean(saved_email));
    let mut out = identity_column(
        live.or(saved).map(str::to_string),
        clean(live_plan).map(str::to_string),
    );
    // Said in the identity column rather than the status word: a home that
    // disagrees with the server about whose it is still serves turns perfectly
    // well, so this is not a reason to call the account unusable. It is a
    // reason to doubt the NAME, and it belongs where the name is.
    if let (Some(l), Some(s)) = (live, saved) {
        if !l.eq_ignore_ascii_case(s) {
            out.push_str(&format!(" (saved as {s})"));
        }
    }
    out
}

/// One Codex account's row, from whichever source could answer.
///
/// Three can, and they fail in different places, which is why all three exist:
///
/// - The account itself, asked directly. Answers per CREDENTIAL, names itself,
///   and is the only one that answers for a home holding no transcripts - the
///   case where there is nothing local to read and the row was a permanent
///   blank. Needs the network and can be throttled.
/// - What the proxy read off a response it was already carrying. Free, and
///   bound to the account that served the turn. Only exists once that account
///   has served something.
/// - The home's transcripts. Free and always there, but bound to the home
///   rather than to a credential, and often hours old.
///
/// The live answer wins, being taken just now. Between the two local ones the
/// NEWER answers rather than a fixed rank: both are honest, and the stale one
/// is simply older. No source at all means no row - shown at zero, an account
/// nobody has measured reads as a full one.
pub fn codex_row(
    home: &str,
    live: Option<&crate::codex_usage::Account>,
    saved_email: Option<&str>,
    seen_by_proxy: Option<crate::quota_cache::Entry>,
    transcript: Option<crate::codex_limits::Limits>,
    now: i64,
) -> Option<(String, crate::tui::Usage)> {
    if let Some(a) = live {
        let mut l = a.limits;
        l.observed_at = Some(now);
        let (name, mut u) = codex_usage_row(home, &l);
        // Why it is refusing, in the account's own words. Beside a window with
        // room left, "out of quota" and "the workspace spend limit is reached"
        // send you to entirely different places.
        u.note = a.refused.as_deref().map(crate::codex_usage::refusal_words);
        // Credits carry an account past a full window, so a window at 100% is
        // not the end of it - unless the cap on those credits is reached too,
        // which is a way through that is closed.
        u.on_credits = a
            .credits
            .as_ref()
            .is_some_and(|c| (c.has_credits || c.unlimited) && !c.overage_limit_reached);
        u.ident = Some(codex_identity(
            a.email.as_deref(),
            a.plan.as_deref(),
            saved_email,
        ));
        return Some((name, u));
    }
    let from_transcript = transcript.map(|l| codex_usage_row(home, &l));
    let from_proxy = seen_by_proxy.map(|e| {
        (
            home.to_string(),
            crate::tui::Usage {
                five_h: e.five_h,
                five_h_reset: e.five_h_reset,
                seven_d: e.seven_d,
                seven_d_reset: e.seven_d_reset,
                observed_at: Some(e.at),
                on_credits: e.on_credits,
                // What the account said would clear it, when it said. A row
                // that can report a refusal and not its remedy sends the
                // reader looking in the wrong place.
                note: e.refused,
                ..Default::default()
            },
        )
    });
    match (from_proxy, from_transcript) {
        (Some(p), Some(t)) => Some(if p.1.observed_at >= t.1.observed_at {
            p
        } else {
            t
        }),
        (p, t) => p.or(t),
    }
}

/// One Codex account's row: the reading, under the name of the home it came from.
///
/// The name is the ONLY thing this can be keyed by, and taking it as the sole
/// argument is the point - a caller cannot substitute a different account for it.
/// An earlier version looked up whoever the switch timeline said was paying when
/// the reading was written, which moved real numbers onto an account that had no
/// transcripts at all.
pub fn codex_usage_row(home: &str, l: &crate::codex_limits::Limits) -> (String, crate::tui::Usage) {
    let p = crate::codex_limits::place(l);
    let u = crate::tui::Usage {
        observed_at: l.observed_at,
        five_h: p.five_h.map(|w| w.used_pct),
        five_h_reset: p.five_h.and_then(|w| w.resets_at),
        seven_d: p.seven_d.map(|w| w.used_pct),
        seven_d_reset: p.seven_d.and_then(|w| w.resets_at),
        ..Default::default()
    };
    (home.to_string(), u)
}

/// Does this row have to say "no login"?
///
/// One place, because there were two and they disagreed. The slot rows asked
/// `has_login`, which knows a Keychain that will not open is not an account
/// nobody signed into; the profile rows asked `slot_token`, the wrapper that
/// throws that distinction away. A profile and a slot sharing a name leaves only
/// the profile row, so on a real machine `rnd` said "no login" beside its own
/// live usage figures.
///
/// A row with no slot behind it has nothing to sign into and never asks.
pub fn row_needs_login(tool: &str, dir: Option<&std::path::Path>) -> bool {
    dir.is_some_and(|d| !crate::proxy::has_login(tool, d))
}

/// `swapdex export [file]` - this machine's account setup, without a single
/// secret in it.
pub fn export(paths: &Paths, out: Option<&std::path::Path>) -> Result<i32> {
    use anyhow::Context;
    let portable = crate::portable::export(paths);
    if !portable.unreadable.is_empty() {
        eprintln!(
            "swapdex: the account registry could not be read ({}) - this export is \
             INCOMPLETE and does not list every account.",
            portable.unreadable.join(", ")
        );
    }
    let text = serde_json::to_string_pretty(&portable)?;
    match out {
        Some(p) => {
            std::fs::write(p, format!("{text}\n"))
                .with_context(|| format!("write {}", p.display()))?;
            println!(
                "wrote {} - names and settings only; every account still signs in on its own machine",
                crate::util::redact_path(&p.display().to_string())
            );
        }
        None => println!("{text}"),
    }
    Ok(0)
}

/// `swapdex import <file>` - re-create that setup here.
pub fn import(paths: &Paths, file: &std::path::Path, dry_run: bool) -> Result<i32> {
    use anyhow::Context;
    let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let incoming: crate::portable::Portable =
        serde_json::from_slice(&bytes).context("that file is not a swapdex export")?;
    if incoming.version > crate::portable::FORMAT_VERSION {
        eprintln!(
            "swapdex: that export was written by a newer swapdex (format {}) - upgrade first",
            incoming.version
        );
        return Ok(2);
    }
    let here: Vec<(String, String)> = crate::adapters::names()
        .into_iter()
        .flat_map(|t| {
            crate::slots::Slots::open_for(paths, t)
                .map(|s| {
                    s.list()
                        .into_iter()
                        .map(|r| (r.name, t.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    let settings = incoming.settings.clone();
    let todo = crate::portable::plan(&here, &incoming);
    if todo.is_empty() {
        println!("every account in that file is already here");
    }
    let mut unknown_tools: Vec<&str> = Vec::new();
    for a in &todo {
        // The tool name goes straight to the slot store, which does not check
        // it, and the display falls back to Claude for anything it does not
        // recognise - so a manifest naming a tool this build has never heard of
        // produced a slot that can never serve, announced as a Claude account.
        // swapdex ships four adapters and is built to grow; a file from a newer
        // build is the ordinary case, and FORMAT_VERSION cannot catch it because
        // adding an adapter is not a format change.
        if crate::adapters::by_name(&a.tool).is_none() {
            eprintln!(
                "swapdex: skipped '{}': this build does not know the tool '{}' - \
                 upgrade swapdex and import again",
                a.name, a.tool
            );
            unknown_tools.push(&a.tool);
            continue;
        }
        if dry_run {
            println!("would create {} ({})", a.name, tool_binary(&a.tool));
            continue;
        }
        match crate::slots::Slots::open_for(paths, &a.tool).and_then(|mut s| s.create(&a.name)) {
            Ok(rec) => {
                crate::slots::link_shared_config(
                    &rec.config_dir,
                    &shared_source(paths, &a.tool),
                    &a.tool,
                );
                println!("created {} ({})", a.name, tool_binary(&a.tool));
            }
            Err(e) => eprintln!("swapdex: could not create '{}': {e}", a.name),
        }
    }
    if let Some(s) = settings {
        if !dry_run {
            crate::settings::save(paths, &s)?;
            println!("settings applied");
        }
    }
    if !todo.is_empty() && !dry_run {
        println!("  each one still needs its own sign-in: `swapdex run <name>`");
    }
    if !unknown_tools.is_empty() {
        // Non-zero so a script does not read a partial import as a clean one.
        return Ok(2);
    }
    Ok(0)
}

/// `swapdex fallback-model [<model>|off]` - what to ask for when every account
/// is past the threshold and there is nowhere left to rotate.
pub fn fallback_model(paths: &Paths, value: Option<&str>) -> Result<i32> {
    let cfg = crate::settings::load(paths);
    let Some(value) = value else {
        match cfg.fallback_model.as_deref() {
            Some(m) => {
                println!("{m}");
                println!("  asked for only when every account is past the threshold");
            }
            None => println!(
                "off - a turn with nowhere left to go gets the refusal, not a different model"
            ),
        }
        return Ok(0);
    };
    if value.eq_ignore_ascii_case("off") || value.is_empty() {
        crate::settings::update(paths, |c| c.fallback_model = None)?;
        println!("fallback model off");
        return Ok(0);
    }

    crate::settings::update(paths, |c| c.fallback_model = Some(value.to_string()))?;
    println!("fallback model: {value}");
    println!("  used ONLY when every account is past the threshold - rotating to an account");
    println!("  with room comes first, because that gives you the model you asked for");
    Ok(0)
}

/// `swapdex strategy [roomiest|consume-first]` - which account auto-continue
/// reaches for when the current one is full.
pub fn strategy(paths: &Paths, value: Option<&str>) -> Result<i32> {
    let cfg = crate::settings::load(paths);
    let Some(value) = value else {
        let s = cfg.strategy();
        println!("{}", s.as_str());
        println!(
            "{}",
            match s {
                crate::proxy::pick::Strategy::Roomiest =>
                    "  reaches for the account with the most left - the largest buffer for a burst",
                crate::proxy::pick::Strategy::ConsumeFirst =>
                    "  reaches for the window about to reset, so quota does not lapse unused",
            }
        );
        return Ok(0);
    };
    let Some(parsed) = crate::proxy::pick::Strategy::parse(value) else {
        eprintln!("swapdex: unknown strategy '{value}' - use `roomiest` or `consume-first`");
        return Ok(2);
    };

    crate::settings::update(paths, |c| {
        c.proxy_strategy = Some(parsed.as_str().to_string())
    })?;
    println!("strategy: {}", parsed.as_str());
    // Read per request, so a proxy already running follows without a restart.
    println!("  a running proxy follows this from its next turn");
    Ok(0)
}

/// `swapdex service install` - hand the proxy to launchd or systemd.
///
/// Two things this fixes, both learned the hard way. A proxy started by the shim
/// dies with the shell that started it, so killing a terminal quietly removes it;
/// and one started over ssh on macOS cannot open the Keychain, so it answers
/// every turn with the client's own login and never says so. An agent runs in the
/// user's own login session, has that access, and is restarted when it stops.
/// What to say after installing the service, once it is known whether it came up.
///
/// The install used to announce "it starts at login, comes back if it stops"
/// without checking that it had. On a Mac whose launchd context cannot open the
/// Keychain the proxy refuses to run - deliberately, because it would forward
/// the user's own login and never say so - so the unit failed every start while
/// swapdex called it installed. The only working proxy on that machine was one
/// started by hand, which is also why it never picked up an upgrade.
fn install_verdict(came_up: bool, tool: &str) -> (bool, String) {
    if came_up {
        return (true, "installed and running".to_string());
    }
    (
        false,
        format!(
            "the unit is written but the proxy did not start. On macOS this is \
             usually the Keychain: a service started by launchd cannot open it, \
             and swapdex refuses to run a proxy that would forward your own login \
             without saying so. Start it from a terminal on this machine instead: \
             `swapdex proxy --ensure --tool {tool}`, then `swapdex shim`"
        ),
    )
}

pub fn service_install(paths: &Paths, sel: Option<ToolSel>) -> Result<i32> {
    let tool = slot_tool(sel);
    let path = crate::service::install(paths, tool)?;
    println!(
        "the {} proxy is now a service: {}",
        tool_binary(tool),
        crate::util::redact_path(&path.display().to_string())
    );
    println!(
        "  it writes what it says to {}",
        crate::util::redact_path(&crate::service::log_dir(paths).display().to_string())
    );
    // Wait for it to actually come up. Announcing that it "starts at login and
    // comes back if it stops" without looking is how a machine ends up with a
    // unit that has failed every start since the day it was installed.
    let mut came_up = false;
    for _ in 0..10 {
        if crate::proxy::running_proxy_for(paths, tool).is_some() {
            came_up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    let (ok, msg) = install_verdict(came_up, tool);
    if ok {
        println!("  {msg}");
        Ok(0)
    } else {
        eprintln!("swapdex: {msg}");
        Ok(2)
    }
}

/// `swapdex service uninstall` - stop it and take the unit away.
pub fn service_uninstall(sel: Option<ToolSel>) -> Result<i32> {
    let tool = slot_tool(sel);
    match crate::service::uninstall(tool)? {
        Some(p) => println!(
            "removed {}",
            crate::util::redact_path(&p.display().to_string())
        ),
        None => println!("no {} service was installed", tool_binary(tool)),
    }
    Ok(0)
}

/// `swapdex service status` - what is installed, and whether it is up.
pub fn service_status(paths: &Paths) -> Result<i32> {
    let home = dirs::home_dir();
    for tool in ["claude-code", "codex"] {
        let path = home.as_ref().map(|h| {
            if cfg!(target_os = "macos") {
                crate::service::launchd_path(h, tool)
            } else {
                crate::service::systemd_path(h, tool)
            }
        });
        let installed = path.as_ref().is_some_and(|p| p.exists());
        let running = crate::proxy::running_proxy_for(paths, tool).is_some();
        println!(
            "{:<12} service: {:<13} proxy: {}",
            tool_binary(tool),
            if installed {
                "installed"
            } else {
                "not installed"
            },
            if running { "running" } else { "not running" }
        );
    }
    println!(
        "  logs: {}",
        crate::util::redact_path(&crate::service::log_dir(paths).display().to_string())
    );
    Ok(0)
}

/// `swapdex refresh --keep-alive` - renew every idle account heading for expiry.
///
/// The same sweep the proxy runs on a timer, exposed so it can be run by hand or
/// from cron on a machine where the proxy is not always up. An account nobody
/// touches is the one that dies: its refresh token goes stale unused, and then
/// only a browser sign-in brings it back.
pub fn keep_alive(paths: &Paths) -> Result<i32> {
    let slots: Vec<(String, std::path::PathBuf)> =
        crate::slots::Slots::open_for(paths, "claude-code")
            .map(|s| {
                s.list()
                    .into_iter()
                    .map(|r| (r.name, r.config_dir))
                    .collect()
            })
            .unwrap_or_default();
    if slots.is_empty() {
        println!("no Claude accounts to keep alive");
        return Ok(0);
    }
    let (renewed, failed) = crate::refresh::keep_alive_sweep(&slots, now_ms());
    for name in &renewed {
        println!("renewed {name}");
    }
    for (name, why) in &failed {
        eprintln!("{}", why.remedy(name));
    }
    if renewed.is_empty() && failed.is_empty() {
        println!("every account has time left - nothing needed renewing");
    }
    // A sweep that could not renew something is worth an exit code, so cron can
    // notice; nothing to do is success.
    Ok(i32::from(!failed.is_empty()) * 4)
}

/// `swapdex refresh [name]` - renew a lapsed access token so the account stays
/// usable without signing in again.
///
/// An access token lives about an hour; an account idle longer than that is not
/// broken, it just needs renewing, and only its own refresh token can do it.
pub fn refresh(paths: &Paths, name: Option<&str>) -> Result<i32> {
    let slots = crate::slots::Slots::open_for(paths, "claude-code")?;
    let list: Vec<_> = match name {
        Some(n) => match slots.get(n) {
            Some(r) => vec![r],
            None => {
                // Name them here rather than sending the reader to another
                // screen to read four words.
                let known: Vec<String> = slots.list().into_iter().map(|r| r.name).collect();
                let refs: Vec<&str> = known.iter().map(String::as_str).collect();
                eprintln!("swapdex: {}", unknown_account(n, &refs));
                return Ok(5);
            }
        },
        None => slots.list(),
    };
    if list.is_empty() {
        println!("No Claude accounts to renew.");
        return Ok(0);
    }
    let now = now_ms();
    let mut renewed = 0;
    for r in &list {
        if !crate::proxy::creds::slot_token_expired(&r.config_dir, now) {
            println!("  {} is already current", r.name);
            continue;
        }
        match crate::refresh::refresh_slot(&r.config_dir, now) {
            Ok(()) => {
                println!("  {} renewed", r.name);
                renewed += 1;
            }
            Err(why) => println!("  {}", why.remedy(&r.name)),
        }
    }
    if renewed > 0 {
        println!("\n{renewed} account(s) renewed - no sign-in needed.");
    }
    Ok(0)
}

/// List the permanent slots (name + the config dir each launches into).
pub fn list_slots(paths: &Paths) -> Result<i32> {
    // Both tools, each under its own heading. Listing only Claude's hid half the
    // accounts from the command whose whole job is to show them.
    let mut any = false;
    for tool in crate::adapters::names() {
        let list = crate::slots::Slots::open_for(paths, tool)?.list();
        if list.is_empty() {
            continue;
        }
        let pointer = crate::slots::Slots::open_for(paths, tool)?.default_dir();
        println!("{tool}:");
        for r in list {
            // The default is what a plain launch of that tool will use, which is
            // the one thing a list of directories cannot otherwise show.
            let mark = if pointer.as_deref() == Some(r.config_dir.as_path()) {
                "*"
            } else {
                " "
            };
            println!("{mark} {}  {}", r.name, r.config_dir.display());
        }
        any = true;
    }
    if !any {
        println!("No accounts yet. Run `swapdex onboard` to set them up,");
        println!("  or launch one directly: swapdex run <name>");
        return Ok(0);
    }
    println!("  (* is the account a plain launch uses)");
    Ok(0)
}

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
    // Take the per-tool credential lock and HOLD it across the whole flow -
    // including the interactive sign-in, during which the store lock (lock1) is
    // released so unrelated ops proceed. This is what stops a concurrent
    // `swapdex use` on THIS tool from interleaving with our sign-out/sign-in and
    // pairing the wrong token with the new account. Held to end of function.
    let _tool_lock = match store.lock_tool(tool) {
        Ok(g) => g,
        Err(_) => {
            eprintln!(
                "swapdex: another swapdex is signing {tool} in/out right now. \
                 Finish or close it, then retry."
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
    // Re-take the STORE lock for the final store-global writes (timeline,
    // profile save). The per-tool credential lock above already excludes a
    // concurrent switch on THIS tool, so this only serializes with other-tool
    // ops - a bounded retry instead of the old single-shot best-effort that
    // could write the timeline unlocked. No other op holds the store lock for
    // long (only login has an interactive wait, and same-tool login is excluded
    // by the per-tool lock), so this settles in well under the bound; the None
    // fallback only avoids discarding the user's completed sign-in in the rare
    // case it truly can't be taken.
    let mut relock = store.lock().ok();
    for _ in 0..25 {
        if relock.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        relock = store.lock().ok();
    }
    let _lock = relock;
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
/// Whether `codex login` should get `--device-auth` (the device-code flow):
/// on by DEFAULT, opt out with `SWAPDEX_CODEX_LOGIN=browser`. Codex's default
/// login is a localhost-redirect browser flow, which needs a browser that can
/// reach localhost on the SAME machine - it fails over SSH / on a headless box
/// (swapdex's common remote use). Pure so the policy is unit-tested.
fn codex_device_auth(opt_out_browser: bool) -> bool {
    !opt_out_browser
}

/// Read the opt-out from the environment: `SWAPDEX_CODEX_LOGIN=browser`.
fn codex_login_opts_out_of_device() -> bool {
    std::env::var("SWAPDEX_CODEX_LOGIN").is_ok_and(|v| v.eq_ignore_ascii_case("browser"))
}

fn spawn_tool_login(bin: &str, tool: &str) -> Result<std::process::ExitStatus> {
    spawn_tool_login_in(std::path::Path::new(bin), tool, None)
}

/// The same sign-in, optionally in one account's own home.
///
/// The dashboard used to build its own invocation here: a BARE launch of
/// whatever `codex` PATH resolved to. That is our shim, and the shim puts the
/// proxy in front of any Codex run it does not recognise as plain - a bare
/// launch is not recognised. So the sign-in talked to the proxy, which answered
/// with the account it was already serving, and an account with no login of its
/// own came up looking signed in. Two ways to build one command is how that
/// happened, so now there is one.
fn spawn_tool_login_in(
    bin: &std::path::Path,
    tool: &str,
    home: Option<(&str, &std::path::Path)>,
) -> Result<std::process::ExitStatus> {
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
    if let Some((var, dir)) = home {
        cmd.env(var, dir);
    }
    // A sign-in must reach the vendor directly: an inherited proxy address
    // answers with whichever account the proxy already holds.
    for var in ["ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY"] {
        cmd.env_remove(var);
    }
    match tool {
        // Codex has a `login` subcommand; Claude Code a proper `auth login`
        // that does JUST the OAuth sign-in (no workspace-trust / session
        // detour); Gemini / Antigravity sign in on first run of the app.
        "codex" => {
            cmd.arg("login");
            // Device-code flow by default so login works over SSH / headless
            // (opt out with SWAPDEX_CODEX_LOGIN=browser). A codex old enough to
            // lack the flag can opt out; current codex ships it.
            if codex_device_auth(codex_login_opts_out_of_device()) {
                cmd.arg("--device-auth");
            }
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
    status.map_err(|e| anyhow::anyhow!("could not run {}: {e}", bin.display()))
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

/// The address a Codex home is signed in as - a label for the row, read with the
/// same decoder the adapter uses, which keeps only the `email` claim.
fn codex_slot_email(dir: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(dir.join("auth.json")).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    adapters::codex::decode_email_from_id_token(v["tokens"]["id_token"].as_str())
}

/// The config dir of the slot registered under `name`, if there is one.
fn slot_dir_named(paths: &Paths, name: &str) -> Option<std::path::PathBuf> {
    crate::slots::Slots::open(paths)
        .ok()?
        .list()
        .into_iter()
        .find(|r| r.name == name)
        .map(|r| r.config_dir)
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
        /// The saved token has already lapsed. Sending it earns a refusal that
        /// looks like the endpoint being busy, so it is never sent.
        expired: bool,
        /// Set when the login could not be READ, carrying why - a locked
        /// keychain is not an account without a login.
        unreadable: Option<String>,
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
            let mut expired = false;
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
                expired = s
                    .part("credentials")
                    .is_some_and(|c| q::credentials_expired(c.expose(), now_ms()));
            }
            // A profile and a slot can carry the same name: the profile is the
            // old copied snapshot, the slot is where that account lives now.
            // Only one of the two is alive - the tool refreshes the slot's
            // credential in place, while nothing refreshes a copy - so the slot
            // answers for the account and the snapshot is not consulted at all.
            let mut unreadable: Option<String> = None;
            if let Some(dir) = slot_dir_named(paths, &p.name) {
                match crate::proxy::creds::slot_token_detail(&dir) {
                    Ok(t) => {
                        token = Some(String::from_utf8_lossy(t.expose()).to_string());
                        expired = crate::proxy::creds::slot_token_expired(&dir, now_ms());
                        email = crate::proxy::creds::slot_email(&dir).or(email);
                        uuid = crate::proxy::creds::slot_account_uuid(&dir).or(uuid);
                    }
                    // Keep WHY. Collapsing a locked keychain into "no token"
                    // tells the user an account they are signed into has no
                    // login - the one reading this over ssh sees that for every
                    // account on the machine.
                    Err(why) => unreadable = Some(why.short().to_string()),
                }
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
                // The live login is refreshed by Claude itself, so only a
                // SNAPSHOT can be stale.
                expired: expired && !active,
                unreadable,
            });
        }
    }
    // Slot accounts too: they hold their own credential (that is the point of the
    // model), so a quota list without them omits exactly the accounts the proxy
    // rotates between. Their token is read from the slot, never copied.
    if let Ok(slots) = crate::slots::Slots::open(paths) {
        for r in slots.list() {
            if rows.iter().any(|x| x.name == r.name) {
                continue;
            }
            let read = crate::proxy::creds::slot_token_detail(&r.config_dir);
            let unreadable = read.as_ref().err().map(|w| w.short().to_string());
            let token = read
                .ok()
                .map(|t| String::from_utf8_lossy(t.expose()).to_string());
            let uuid = crate::proxy::creds::slot_account_uuid(&r.config_dir);
            let active = live_uuid.is_some() && uuid == live_uuid;
            matched_live |= active;
            rows.push(Row {
                label: if active {
                    format!("{} (active)", r.name)
                } else {
                    r.name.clone()
                },
                name: r.name.clone(),
                email: crate::proxy::creds::slot_email(&r.config_dir),
                token,
                active,
                // A slot's token is renewable, so renew it rather than report it
                // dead - an account idle for an hour is not an account with a
                // problem, and it is usually the one with quota left.
                expired: {
                    if crate::proxy::creds::slot_token_expired(&r.config_dir, now_ms()) {
                        let _ = crate::refresh::refresh_slot(&r.config_dir, now_ms());
                    }
                    crate::proxy::creds::slot_token_expired(&r.config_dir, now_ms())
                },
                unreadable,
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
                expired: false,
                unreadable: None,
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
            // A machine can hold only Codex accounts, and returning here left
            // it with nothing to show but a note about a tool it does not use.
            println!();
            print_codex_quota(paths, now_secs() as i64);
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
    let mut to_fetch: Vec<(usize, String)> = Vec::new();
    let mut offline: Option<String> = None;
    for (i, r) in rows.iter().enumerate() {
        match &r.token {
            None => results.push((
                i,
                Fetch::Offline(
                    r.unreadable
                        .clone()
                        .unwrap_or_else(|| "no saved token".into()),
                ),
            )),
            // Do not ask on behalf of a token that has already lapsed. The
            // endpoint refuses it the same way it refuses a burst, so the answer
            // would read as "busy, try again in a moment" - advice that can never
            // come true, and three retries per dead account that make the
            // endpoint busier for the accounts that CAN answer.
            Some(_) if r.expired => results.push((
                i,
                Fetch::Offline(
                    "saved token expired - snapshots go stale as refresh tokens rotate; \
                     `swapdex run <name>` gives this account a slot that stays fresh"
                        .into(),
                ),
            )),
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
            // Collected and read together below: one round trip per account in
            // sequence was most of the wait.
            Some(t) => to_fetch.push((i, t.clone())),
        }
    }
    if !to_fetch.is_empty() {
        let got = q::fetch_many(to_fetch);
        // If NOTHING reached the endpoint, this is the machine being offline
        // rather than a per-account problem, and saying so once beats saying it
        // per account.
        let any_reached = got.iter().any(|(_, f)| !matches!(f, Fetch::Offline(_)));
        if !any_reached {
            if let Some((_, Fetch::Offline(msg))) = got.first() {
                offline = Some(msg.clone());
            }
        }
        results.extend(got);
    }
    results.sort_by_key(|(i, _)| *i);

    // Remember what came back, here where the reading is actually taken - every
    // caller of this command then benefits, including the dashboard, which cannot
    // record a reading it never received.
    let remembered: Vec<(String, crate::quota_cache::Entry)> = results
        .iter()
        .filter_map(|(i, f)| match f {
            Fetch::Ok(qd) => Some((
                rows[*i].name.clone(),
                crate::quota_cache::Entry {
                    five_h: qd.five_hour.map(|w| w.used_pct),
                    five_h_reset: qd.five_hour.and_then(|w| w.resets_at),
                    seven_d: qd.seven_day.map(|w| w.used_pct),
                    seven_d_reset: qd.seven_day.and_then(|w| w.resets_at),
                    at: now,
                    on_credits: qd.can_serve_past_windows(),
                    refused: None,
                },
            )),
            _ => None,
        })
        .collect();
    crate::quota_cache::update(paths, &remembered);

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
            Fetch::Throttled => println!("  {}", throttled_note()),
        }
        println!();
    }
    print_codex_quota(paths, now);
    println!("this is the only swapdex command that touches the network.");
    Ok(0)
}

/// What to say when the usage endpoint declined to answer.
///
/// A 429 is the ENDPOINT being busy, and `quota.rs` classifies it that way with
/// a test named for it. This line then said "the account is fine" - the same
/// mistake mirrored: avoiding a false alarm by making a false reassurance. A
/// real account that was spent and refusing every turn printed exactly that
/// while the user tried to work out why nothing went through. A declined read
/// is no reading; it is not evidence in either direction.
fn throttled_note() -> &'static str {
    "usage endpoint declined to answer just now - that is no reading, \
     not a verdict on this account either way; try again in a moment"
}

/// The Codex login held in a saved snapshot, for accounts with no slot.
///
/// A snapshot is a copy, so its token can be older than the account's live one
/// and the endpoint may refuse it - which is reported as a refusal rather than
/// as an account with no numbers.
fn snapshot_codex_auth(paths: &Paths, name: &str) -> Option<crate::proxy::codex::Auth> {
    let snap = Store::open(paths).ok()?.load(name, "codex").ok()??;
    let v: Value = serde_json::from_slice(snap.part("auth")?.expose()).ok()?;
    let token = v["tokens"]["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    let id = v["tokens"]["account_id"].as_str().unwrap_or_default();
    Some(crate::proxy::codex::Auth {
        token: crate::secret::Secret::new(token.as_bytes().to_vec()),
        account_id: id.to_string(),
    })
}

/// Every Codex account this machine knows, and where to read it from.
///
/// An account can be a SLOT (its own directory, the live login) or a saved
/// SNAPSHOT (a copy in swapdex's store). The report walked slots only, so a
/// machine that keeps its accounts as snapshots got no Codex section at all and
/// not a word saying why - found on a real one, where `ls` listed three Codex
/// accounts and `quota` printed nothing about any of them.
///
/// `Some(dir)` is a slot and is preferred where an account is held both ways:
/// the slot is what the tool refreshes, the snapshot a copy that goes stale.
pub fn codex_account_sources(
    slots: &[(String, std::path::PathBuf)],
    snapshots: &[String],
) -> Vec<(String, Option<std::path::PathBuf>)> {
    let mut out: Vec<(String, Option<std::path::PathBuf>)> = slots
        .iter()
        .map(|(n, d)| (n.clone(), Some(d.clone())))
        .collect();
    for n in snapshots {
        if !out.iter().any(|(have, _)| have == n) {
            out.push((n.clone(), None));
        }
    }
    out
}

/// The Codex half of `swapdex quota`.
///
/// Kept as its own pass rather than folded into the Claude loop above: the two
/// answer different endpoints with different shapes, and the only thing they
/// share is how a window is drawn. Silent when there are no Codex accounts, so
/// a Claude-only machine sees no empty heading.
fn print_codex_quota(paths: &Paths, now: i64) {
    let slots: Vec<(String, std::path::PathBuf)> = crate::slots::Slots::open_for(paths, "codex")
        .map(|s| {
            s.list()
                .into_iter()
                .map(|r| (r.name, r.config_dir))
                .collect()
        })
        .unwrap_or_default();
    let snapshots: Vec<String> = Store::open(paths)
        .map(|st| {
            st.list()
                .into_iter()
                .filter(|p| p.tools.iter().any(|t| t == "codex"))
                .map(|p| p.name)
                .collect()
        })
        .unwrap_or_default();
    let accounts = codex_account_sources(&slots, &snapshots);
    if accounts.is_empty() {
        return;
    }
    println!("codex - remaining on your Codex accounts");
    println!("live from ChatGPT's usage endpoint; opt-in network, spends 0 message quota.\n");
    for (name, dir) in &accounts {
        let saved = dir.as_deref().and_then(codex_slot_email);
        // A slot reads from its own directory; a snapshot from the copy in the
        // store. Reading only the first left snapshot-only machines with an
        // empty section and no reason for it.
        let auth = match dir {
            Some(d) => crate::proxy::codex::slot_auth(d),
            None => snapshot_codex_auth(paths, name),
        };
        match auth {
            None => {
                println!("{name}");
                println!(
                    "  no readable Codex login - `swapdex run {name} --tool codex` once signs it in"
                );
            }
            Some(auth) => match crate::codex_usage::fetch(&auth) {
                crate::codex_usage::Fetch::Ok(a) => {
                    println!(
                        "{name}   {}",
                        codex_identity(a.email.as_deref(), a.plan.as_deref(), saved.as_deref())
                    );
                    for line in codex_quota_lines(&a, now) {
                        println!("  {line}");
                    }
                }
                // Each failure keeps its own name for the same reason it does on
                // the Claude side: a busy endpoint and a dead login are different
                // news, and one silence for both hides whichever matters.
                f => {
                    println!("{name}   {}", saved.unwrap_or_default());
                    println!("  {}", f.why_no_number().unwrap_or("no reading"));
                }
            },
        }
        println!();
    }
}

/// What `swapdex quota` prints for one Codex account, beyond its name.
///
/// A dashboard row is one line, so the endpoint's per-model windows, its credit
/// balance and its refusal reason had nowhere to go. They go here, on the
/// surface that already prints a line per window.
///
/// Nothing is claimed that the response did not say. An account it said nothing
/// about prints that it said nothing, rather than an encouraging blank.
fn codex_quota_lines(a: &crate::codex_usage::Account, now: i64) -> Vec<String> {
    let as_window = |w: &crate::codex_limits::Window| crate::quota::Window {
        used_pct: w.used_pct,
        resets_at: w.resets_at,
    };
    let mut out = Vec::new();
    let placed = crate::codex_limits::place(&a.limits);
    // Per-model names run far past the width a window label is normally given
    // ("GPT-5.3-Codex-Spark" against "7d"), so the labels are padded to the
    // widest of THIS account's before they are drawn. Without it the bars step
    // sideways down the block and stop reading as one column.
    let pad = a
        .scoped
        .iter()
        .map(|(l, _)| l.chars().count())
        .chain(std::iter::once(9))
        .max()
        .unwrap_or(9);
    let line = |label: &str, w: &crate::codex_limits::Window| {
        win_line(&format!("{label:<pad$}"), &as_window(w), now)
    };
    for (label, w) in [("5h", placed.five_h), ("7d", placed.seven_d)] {
        if let Some(w) = w {
            out.push(line(label, &w));
        }
    }
    for (label, w) in &a.scoped {
        out.push(line(label, w));
    }
    if out.is_empty() {
        out.push("(the endpoint reported no windows - `swapdex quota --json` to inspect)".into());
    }
    // Whether a full window is a pause or the end of this account.
    if let Some(c) = &a.credits {
        out.push(match c {
            _ if c.unlimited => "credits: unlimited".to_string(),
            _ if c.overage_limit_reached => {
                "credits: spend limit reached - a full window is the end until it is raised".into()
            }
            _ if c.has_credits => match &c.balance {
                Some(b) => format!("credits: {b} - a full window is not the end"),
                None => "credits available - a full window is not the end".into(),
            },
            _ => "no credits - a full window is the end of this account".to_string(),
        });
    }
    if let Some(kind) = &a.refused {
        out.push(format!(
            "refusing turns: {}",
            crate::codex_usage::refusal_words(kind)
        ));
    }
    out
}

/// Render one window as a remaining-percent bar with its reset countdown.
fn win_line(label: &str, w: &crate::quota::Window, now: i64) -> String {
    let rem = w.remaining_pct();
    let filled = ((rem / 100.0) * 10.0).round().clamp(0.0, 10.0) as usize;
    let bar: String = "\u{2593}".repeat(filled) + &"\u{2591}".repeat(10 - filled);
    // The time, not the wait. A countdown has to be recomputed to stay true, so
    // it decays the moment this output is scrolled back to or piped to a file,
    // while a clock stays right - and 몇시에 리셋인지가 몇시간 남았는지보다
    // 머리에 남는다. Both first-party CLIs print reset times this way.
    let reset = match w.resets_at {
        Some(ts) => format!(
            "   resets {}",
            crate::proxy::pick::reset_clock(ts, now, crate::proxy::tz_offset())
        ),
        None => String::new(),
    };
    format!("{label:<9} {bar}  {rem:>3.0}% left{reset}")
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
            // Full windows are not the end of an account that can bill credits.
            m.insert("on_credits".into(), Value::Bool(q.can_serve_past_windows()));
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
        Fetch::Throttled => {
            m.insert("status".into(), Value::String("throttled".into()));
            m.insert(
                "note".into(),
                Value::String("the usage endpoint is rate-limited, not this account".into()),
            );
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
    use super::{
        best_identity, codex_account_sources, codex_identity, codex_quota_lines, codex_row,
        codex_usage_row, home_note, keychain_verdict, listable, payer_line, payer_note,
        payer_of_any, pick_active, quota_brief, row_needs_login, row_suffix, sign_in_remedy,
        stale_hint, stale_marker, switch_line, unhonoured_ask, unknown_account, win_line,
    };

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    #[test]
    fn win_line_shows_remaining_bar_and_reset() {
        let w = crate::quota::Window {
            used_pct: 61.0,
            resets_at: Some(2 * 3600 + 14 * 60),
        };
        // A clock, not a countdown: this output is written once and may be
        // read, scrolled back to, or piped long after. "resets in 2h 14m" is
        // true for one second; the time it names stays true.
        let line = win_line("5h", &w, 0);
        assert!(line.contains("39% left"), "{line}");
        assert!(line.contains("resets"), "{line}");
        assert!(
            line.contains("am") || line.contains("pm"),
            "the reset is a time: {line}"
        );
        assert!(!line.contains("in 2h"), "no countdown: {line}");
        let full = crate::quota::Window {
            used_pct: 0.0,
            resets_at: None,
        };
        let line = win_line("7d", &full, 0);
        assert!(line.contains("100% left"), "{line}");
        assert!(!line.contains("resets"), "no reset when absent: {line}");
    }

    /// The two row builders asked this question differently, and the lossy one
    /// won on any account with both a profile and a slot: `rnd` said "no login"
    /// beside its own live usage figures, because a Keychain that would not open
    /// read as an account nobody had signed into. One helper now, so they cannot
    /// drift apart again.
    #[test]
    fn a_row_with_a_readable_login_does_not_ask_for_one() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing there yet: it needs a login, and says so.
        assert!(row_needs_login("claude-code", Some(dir.path())));
        // A credential in the slot: nothing to ask for.
        std::fs::write(
            dir.path().join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"T"}}"#,
        )
        .unwrap();
        assert!(!row_needs_login("claude-code", Some(dir.path())));
        // A row with no slot behind it has nothing to sign into.
        assert!(!row_needs_login("claude-code", None));
    }

    /// Codex prints the provider name and nothing else about identity, so that
    /// one field has to answer "which account am I on". It carried the SLOT
    /// NAME - `swapdex: work` - which is a label its owner chose and says
    /// nothing about the login being billed. 병승 asked exactly that question
    /// looking straight at the line meant to answer it.
    #[test]
    fn the_payer_line_names_the_account_not_just_the_slot() {
        assert_eq!(
            payer_line("work", Some("polarisairnd@gmail.com"), true),
            "work (polarisairnd@gmail.com)"
        );
        // No email to show (Codex slot never signed in, or Claude before its
        // first read): the name alone, never a blank or a lie.
        assert_eq!(payer_line("work", None, true), "work");
        // A name that IS the address does not repeat itself.
        assert_eq!(payer_line("me@x.com", Some("me@x.com"), true), "me@x.com");
        // The state still rides along, in the shape callers already read.
        assert_eq!(
            payer_line("work", Some("a@b.c"), false),
            "work (a@b.c, no login)"
        );
        assert_eq!(payer_line("work", None, false), "work (no login)");
    }

    /// Two pointers on purpose - `serve` decides who pays, `use` decides where
    /// sessions live - but Codex shows ONE field, so a session billed to `work`
    /// while its history piled up in `codex-main` read as though it were running
    /// as `work`. 병승 asked whether it had actually gone into that slot.
    #[test]
    fn the_home_is_named_only_when_it_differs_from_the_payer() {
        assert_eq!(home_note("work", Some("codex-main")), " - home: codex-main");
        assert_eq!(
            home_note("work", Some("work")),
            "",
            "nothing to disambiguate when they agree"
        );
        assert_eq!(home_note("work", None), "", "no pointer, nothing to say");
    }

    /// A Codex account can be a slot OR a saved snapshot, and the report used
    /// to walk slots only - so a machine that keeps its accounts as snapshots
    /// got no Codex section at all, and not a word saying why. Found on a real
    /// one: `swapdex ls` listed three Codex accounts and `swapdex quota`
    /// printed nothing about any of them.
    #[test]
    fn codex_accounts_are_gathered_from_slots_and_snapshots_alike() {
        let got = codex_account_sources(
            &[("live".into(), std::path::PathBuf::from("/slots/live"))],
            &["saved".into(), "live".into()],
        );
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"live"), "{names:?}");
        assert!(
            names.contains(&"saved"),
            "a snapshot is an account too: {names:?}"
        );
        // An account held both ways is one account, listed once - and the slot
        // is the live copy, so it wins.
        assert_eq!(names.iter().filter(|n| **n == "live").count(), 1);
        assert!(
            got.iter().any(|(n, src)| n == "live" && src.is_some()),
            "the slot keeps its directory: {got:?}"
        );
        assert!(
            got.iter().any(|(n, src)| n == "saved" && src.is_none()),
            "a snapshot has no directory to read: {got:?}"
        );
    }

    /// The status bar needs the payer's remaining quota without a network call.
    ///
    /// Going through the proxy costs the tool its own `rate_limits` block, so a
    /// status line that reads them prints "weekly N/A | 5h N/A" while swapdex
    /// holds a reading from a minute ago. That reading should be available
    /// instantly - a bar redraws constantly and cannot wait on a request.
    #[test]
    fn the_payer_quota_renders_from_cache_without_a_request() {
        assert_eq!(quota_brief(Some(17.0), Some(98.0)), "5h 83% | 7d 2%");
        // One window unknown: report the one that is known.
        assert_eq!(quota_brief(None, Some(50.0)), "7d 50%");
        assert_eq!(quota_brief(Some(0.0), None), "5h 100%");
        // Nothing measured: empty, so the bar can omit the segment entirely
        // rather than print a placeholder that looks like a reading.
        // Silence here is indistinguishable from a broken status bar - which is
        // exactly how it read when the numbers stopped appearing.
        assert_eq!(quota_brief(None, None), "usage unread");
    }

    /// Whoever pays for ANY tool gets marked, not just Claude's payer.
    ///
    /// `ls` asked `Slots::open_for(paths, "claude-code")` who pays, so serving a
    /// Codex account moved the turns correctly and the listing marked nobody -
    /// the same "the switch did nothing" appearance that was fixed for Claude in
    /// 0.80.0, still live on the Codex side.
    #[test]
    fn a_payer_of_any_tool_is_marked() {
        // Claude pays: as before.
        assert_eq!(
            payer_of_any(&[("claude-code", Some("alpha")), ("codex", None)]).as_deref(),
            Some("alpha")
        );
        // Only Codex has a payer: it gets marked.
        assert_eq!(
            payer_of_any(&[("claude-code", None), ("codex", Some("cx"))]).as_deref(),
            Some("cx")
        );
        // Both: Claude's answer is the one a single mark can carry, and it is
        // the tool `ls` is otherwise about.
        assert_eq!(
            payer_of_any(&[("claude-code", Some("alpha")), ("codex", Some("cx"))]).as_deref(),
            Some("alpha")
        );
        // Nobody: nothing marked.
        assert_eq!(
            payer_of_any(&[("claude-code", None), ("codex", None)]),
            None
        );
        assert_eq!(payer_of_any(&[]), None);
    }

    /// The remedy must name the TOOL, or it sends the reader to the wrong one.
    ///
    /// A Codex account that could not serve was told to run `swapdex run
    /// codex-test`, and `run` defaults to Claude - so following the instruction
    /// launched Claude for a Codex profile. The advice has to carry the tool it
    /// is about.
    #[test]
    fn the_sign_in_remedy_names_the_tool() {
        assert_eq!(
            sign_in_remedy("codex-test", "codex"),
            "`swapdex run codex-test --tool codex` signs it in once"
        );
        // Claude is the default, so naming it would be noise.
        assert_eq!(
            sign_in_remedy("alpha", "claude-code"),
            "`swapdex run alpha` signs it in once"
        );
        assert_eq!(
            sign_in_remedy("g", "gemini"),
            "`swapdex run g --tool gemini` signs it in once"
        );
    }

    /// A name that does not match should show the names that do.
    ///
    /// A typo answered "no account named 'alicee' - `swapdex ui` lists them",
    /// sending the user to open another screen to read four words. The list is
    /// right there; and when one candidate is an obvious near-miss, saying so
    /// is the whole answer.
    #[test]
    fn a_wrong_name_is_answered_with_the_right_ones() {
        // Near-miss: name it, because that is almost certainly the intent.
        assert_eq!(
            unknown_account("alicee", &["alice", "bob"]),
            "no account named 'alicee' - did you mean 'alice'? (also: bob)"
        );
        // No near-miss: just list them.
        assert_eq!(
            unknown_account("zzz", &["alice", "bob"]),
            "no account named 'zzz' - you have: alice, bob"
        );
        // Nothing saved at all: say that, not an empty list.
        assert_eq!(
            unknown_account("alice", &[]),
            "no accounts saved yet - `swapdex add <name>` saves the login you are on"
        );
    }

    /// Switching should not need a second command to confirm it.
    ///
    /// `serve rnd` printed "turns -> rnd" and two lines of explanation, and
    /// said nothing about the account itself - so every switch was followed by
    /// `ls` to see whether it took, and by `usage` to see whether that account
    /// had any room. The confirmation belongs in the switch.
    #[test]
    fn a_switch_confirms_itself_with_the_account_it_moved_to() {
        // Identity and room both known: one line carries both.
        assert_eq!(
            switch_line("rnd", Some("rnd@x.com"), Some(68.0)),
            "now rnd (rnd@x.com) - 68% of the week left"
        );
        // Room unknown - never measured, or the window has not been read.
        // Say the identity and stay silent about what is not known.
        assert_eq!(
            switch_line("rnd", Some("rnd@x.com"), None),
            "now rnd (rnd@x.com)"
        );
        // A slot with no config to name it: the name is all there is.
        assert_eq!(
            switch_line("bsgong", None, Some(12.0)),
            "now bsgong - 12% of the week left"
        );
        assert_eq!(switch_line("bsgong", None, None), "now bsgong");
        // Spent: worth saying plainly rather than printing "0% left".
        assert_eq!(
            switch_line("kong", Some("k@x.com"), Some(0.0)),
            "now kong (k@x.com) - no week left"
        );
    }

    /// An account listed from its slot still shows whose it is.
    ///
    /// Identity was read from the saved snapshot only, so an account that
    /// exists as a slot and was never snapshotted had an empty name column -
    /// the row was there and switching worked, but there was no way to tell
    /// which login it was. The slot's own `.claude.json` says.
    #[test]
    fn a_slot_only_account_still_shows_its_email() {
        // Snapshot knows: that answer stands.
        assert_eq!(
            best_identity(Some("snap@x.com".into()), Some("slot@x.com".into())).as_deref(),
            Some("snap@x.com")
        );
        // Snapshot silent, slot knows: use the slot.
        assert_eq!(
            best_identity(None, Some("slot@x.com".into())).as_deref(),
            Some("slot@x.com")
        );
        // Neither knows: still nothing, rather than a guess.
        assert_eq!(best_identity(None, None), None);
    }

    /// Every switchable account needs a row, or a switch to it shows nothing.
    ///
    /// `ls` listed saved snapshots only. On a machine whose accounts live as
    /// SLOTS, `serve personal` moved the turns correctly and the list had no
    /// row for `personal` at all - so the mark saying who pays had nowhere to
    /// appear, and two of three switches looked like they did nothing.
    #[test]
    fn every_switchable_account_gets_a_row() {
        // Snapshots and slots are merged, and neither is listed twice.
        assert_eq!(
            listable(&["claude", "rnd", "work"], &["rnd", "personal", "bsgong"]),
            vec!["bsgong", "claude", "personal", "rnd", "work"]
        );
        // Slots only: still a full list.
        assert_eq!(listable(&[], &["personal"]), vec!["personal"]);
        // Snapshots only: unchanged from before.
        assert_eq!(listable(&["claude"], &[]), vec!["claude"]);
        assert!(listable(&[], &[]).is_empty());
    }

    /// The list must show the switch that was just made.
    ///
    /// `serve rnd` says "turns -> rnd", and then `ls` starred a different
    /// account - the one holding the login on disk - with the paying account
    /// named nowhere. Switching looked like it had not taken, which is exactly
    /// what its owner concluded, repeatedly.
    #[test]
    fn the_list_shows_which_account_pays() {
        // Signed in as kong, paying with rnd: the row for rnd says so.
        assert_eq!(row_suffix("rnd", Some("kong"), Some("rnd")), Some("pays"));
        // ...and the row for kong is not marked as paying.
        assert_eq!(row_suffix("kong", Some("kong"), Some("rnd")), None);
        // The payer is marked EVEN WHEN it also holds the login. Suppressing
        // it there meant that of three accounts, switching to one of them
        // produced no visible change at all - reading as that switch failing.
        assert_eq!(row_suffix("kong", Some("kong"), Some("kong")), Some("pays"));
        // Nothing paying (no proxy): nothing to say.
        assert_eq!(row_suffix("kong", Some("kong"), None), None);
        // A row that is neither is never marked.
        assert_eq!(row_suffix("bsgong", Some("kong"), Some("rnd")), None);
    }

    /// `ls` marks the account Claude is signed in AS. When a proxy is paying
    /// with a different one, that mark is true and misleading at once: the
    /// owner switched to bsgong, the proxy served every turn from bsgong, and
    /// the list went on starring kong because that is whose login the tool
    /// holds locally. Both facts are real; showing only one reads as the switch
    /// having failed.
    #[test]
    fn the_list_separates_who_is_signed_in_from_who_pays() {
        // Different accounts: both are named, and which is which is explicit.
        assert_eq!(
            payer_note(Some("kong"), Some("bsgong")).as_deref(),
            Some("bsgong pays")
        );
        // The same account: nothing to disambiguate, so nothing is added.
        assert_eq!(payer_note(Some("kong"), Some("kong")), None);
        // No proxy paying: the login is the whole story.
        assert_eq!(payer_note(Some("kong"), None), None);
        // Signed into nothing, but a proxy is paying - still worth saying.
        assert_eq!(
            payer_note(None, Some("bsgong")).as_deref(),
            Some("bsgong pays")
        );
    }

    /// Naming the stale tool was half the job: the reader still has to know
    /// what to DO. `add --update` re-saves a snapshot from a live login, which
    /// is no help when the login itself is what has lapsed - the tool has to be
    /// signed into first, and that is per-tool.
    ///
    /// It also has to say the account is still usable for its OTHER tools,
    /// because a lone marker beside an account reads as "this account is
    /// broken" when Claude and Codex are working perfectly well.
    #[test]
    fn a_stale_hint_says_what_to_do_per_tool_and_what_still_works() {
        let h = stale_hint(&["gemini", "antigravity"], &["claude-code", "codex"]);
        assert!(h.contains("gemini"), "{h}");
        assert!(h.contains("antigravity"), "{h}");
        // The fix is signing that tool in, not re-saving a snapshot.
        assert!(
            !h.contains("add --update"),
            "add --update cannot refresh a login that has lapsed: {h}"
        );
        // And it says the account still serves its healthy tools.
        assert!(
            h.contains("claude-code") && h.contains("codex"),
            "must say what still works: {h}"
        );
    }

    /// Nothing stale, nothing said.
    #[test]
    fn no_stale_tool_means_no_hint() {
        assert!(stale_hint(&[], &["claude-code"]).is_empty());
    }

    /// Every tool stale: there is nothing to reassure the reader about, and
    /// claiming otherwise would be the opposite failure.
    #[test]
    fn all_tools_stale_promises_nothing() {
        let h = stale_hint(&["gemini"], &[]);
        assert!(h.contains("gemini"), "{h}");
        assert!(!h.contains("still"), "nothing is still working: {h}");
    }

    /// One tool going stale is not the account going stale. A profile holding
    /// four logins was marked `(stale)` whole because gemini had not been
    /// refreshed in 37 days, while the Codex login added minutes earlier
    /// answered the server perfectly well - so the row said "unusable" about an
    /// account that was working.
    #[test]
    fn a_stale_marker_names_the_tool_it_belongs_to() {
        assert_eq!(
            stale_marker(&[("claude-code", None), ("gemini", Some("stale"))]).as_deref(),
            Some("gemini stale"),
            "name the one that is stale, not the account"
        );
        // Several: named together, so the reader knows the whole extent.
        assert_eq!(
            stale_marker(&[("gemini", Some("stale")), ("codex", Some("expired"))]).as_deref(),
            Some("gemini stale, codex expired")
        );
        // Every tool stale IS the account being stale, and says so plainly.
        assert_eq!(
            stale_marker(&[("gemini", Some("stale")), ("codex", Some("stale"))]).as_deref(),
            Some("gemini stale, codex stale")
        );
        // Nothing wrong: nothing said.
        assert_eq!(stale_marker(&[("claude-code", None)]), None);
        assert_eq!(stale_marker(&[]), None);
    }

    /// Everything the endpoint says that a one-line row has no room for: the
    /// per-model windows, the credit balance, and the refusal reason.
    #[test]
    fn the_codex_report_prints_what_a_row_cannot_hold() {
        let w = |pct: f64, mins: i64| crate::codex_limits::Window {
            used_pct: pct,
            window_minutes: mins,
            resets_at: Some(1_787_196_620),
        };
        let a = crate::codex_usage::Account {
            email: Some("someone@example.com".into()),
            plan: Some("pro".into()),
            limits: crate::codex_limits::Limits {
                short: Some(w(84.0, 10080)),
                long: None,
                observed_at: None,
            },
            scoped: vec![("GPT-5.3-Codex-Spark".into(), w(40.0, 10080))],
            credits: Some(crate::codex_usage::Credits {
                has_credits: false,
                unlimited: false,
                overage_limit_reached: false,
                balance: Some("0".into()),
            }),
            refused: Some("workspace_member_credits_depleted".into()),
        };
        let out = codex_quota_lines(&a, 1_786_600_000).join("\n");

        // The plan window, labelled by its LENGTH rather than by which field
        // carried it.
        assert!(out.contains("7d"), "{out}");
        assert!(!out.contains("5h"), "a window Codex did not send: {out}");
        // The per-model window, under the name the endpoint gave it.
        assert!(out.contains("GPT-5.3-Codex-Spark"), "{out}");
        // Every bar starts at one column, whatever the labels' lengths.
        let starts: Vec<_> = out
            .lines()
            .filter(|l| l.contains('\u{2593}') || l.contains('\u{2591}'))
            .map(|l| l.find(['\u{2593}', '\u{2591}']).unwrap())
            .collect();
        assert_eq!(starts.len(), 2, "{out}");
        assert_eq!(starts[0], starts[1], "bars must line up: {out}");
        // A balance of zero is a fact worth printing: it is the difference
        // between a full window being a pause and being the end.
        assert!(out.contains("no credits"), "{out}");
        // And who can clear the refusal.
        assert!(out.contains("its owner has to top them up"), "{out}");
    }

    /// Silence is not the same as good news. An account the endpoint said
    /// nothing about must not print a reassuring blank.
    #[test]
    fn a_codex_report_with_no_windows_says_so() {
        let a = crate::codex_usage::Account::default();
        let out = codex_quota_lines(&a, 1_786_600_000).join("\n");
        assert!(out.contains("no windows"), "{out}");
        // Nothing was said about credits, so nothing is claimed about them.
        assert!(!out.contains("credits"), "{out}");
    }

    /// Asking for an account and getting it are different things. The ask wins
    /// at first, so pressing Enter shows the choice immediately instead of
    /// lagging a turn behind. But when the proxy has SERVED someone else since
    /// the ask, the ask did not take - and a row that goes on naming it is
    /// telling the user their key worked when it did not.
    ///
    /// Seen on a real machine: `rnd` was asked for at 13:45, every turn after
    /// went to `bsgong` because rnd refuses on overage, and the dashboard said
    /// `rnd active 95% left` for half an hour.
    #[test]
    fn reality_outranks_the_ask_once_the_proxy_has_acted_on_it() {
        let ask = || Some("rnd".to_string());
        let did = || Some("bsgong".to_string());

        // Just asked, proxy has not served since: show the choice.
        assert_eq!(
            pick_active(ask(), did(), None, false).as_deref(),
            Some("rnd")
        );
        // The proxy served someone else AFTER the ask: that is what is happening.
        assert_eq!(
            pick_active(ask(), did(), None, true).as_deref(),
            Some("bsgong")
        );
        // Agreement needs no adjudication.
        assert_eq!(
            pick_active(ask(), ask(), None, true).as_deref(),
            Some("rnd")
        );
        // Nobody asked: the proxy's own record is the only answer there is.
        assert_eq!(
            pick_active(None, did(), Some("x".into()), false).as_deref(),
            Some("bsgong")
        );
        // Nothing anywhere: fall back to where sessions start.
        assert_eq!(
            pick_active(None, None, Some("x".into()), false).as_deref(),
            Some("x")
        );
    }

    /// And the divergence is stated, not merely resolved silently: the user
    /// asked for something and needs to know it is not happening, and why the
    /// screen names a different account than the one they picked.
    #[test]
    fn a_thwarted_ask_is_said_out_loud() {
        assert_eq!(
            unhonoured_ask(Some("rnd"), Some("bsgong"), true).as_deref(),
            Some("asked for rnd - it cannot serve, so turns are going to bsgong")
        );
        // Honoured, or not yet acted on: nothing to say.
        assert_eq!(unhonoured_ask(Some("rnd"), Some("bsgong"), false), None);
        assert_eq!(unhonoured_ask(Some("rnd"), Some("rnd"), true), None);
        assert_eq!(unhonoured_ask(None, Some("bsgong"), true), None);
    }

    /// The endpoint says whose token this is; the local `id_token` says whose
    /// the home believes it is. When they disagree the LIVE answer is the true
    /// one, and the disagreement itself is the news - it is the shape of the
    /// identity mix-up where signing in as one account leaves another connected.
    #[test]
    fn a_live_identity_outranks_the_saved_one_and_a_mismatch_is_reported() {
        assert_eq!(
            codex_identity(Some("a@example.com"), Some("pro"), Some("a@example.com")),
            "a@example.com [pro]"
        );
        assert_eq!(
            codex_identity(
                Some("live@example.com"),
                Some("pro"),
                Some("saved@example.com")
            ),
            "live@example.com [pro] (saved as saved@example.com)"
        );

        // With no live answer the saved label stands, unqualified - it is all
        // there is, and marking it as suspect would be inventing a doubt.
        assert_eq!(
            codex_identity(None, None, Some("saved@example.com")),
            "saved@example.com"
        );

        // A live answer with no saved one to compare is not a mismatch.
        assert_eq!(
            codex_identity(Some("live@example.com"), None, None),
            "live@example.com"
        );

        // The plan is the column Codex rows have always left empty; the
        // endpoint is the only thing that has ever stated it.
        assert!(codex_identity(Some("a@example.com"), Some("pro"), None).ends_with("[pro]"));
    }

    /// A reading the proxy took carries the refusal and the credits it came
    /// with, not just the numbers. Dropping them left a row able to say an
    /// account was refusing and unable to say what would clear it.
    #[test]
    fn a_proxy_reading_brings_its_reason_and_its_credits_along() {
        let seen = crate::quota_cache::Entry {
            seven_d: Some(100.0),
            at: 1_786_590_000,
            on_credits: true,
            refused: Some("out of quota".into()),
            ..Default::default()
        };
        let (_, u) =
            codex_row("work", None, None, Some(seen), None, 1_786_600_000).expect("a reading");
        assert_eq!(u.note.as_deref(), Some("out of quota"));
        assert!(u.on_credits);
    }

    /// Three sources can answer, and between the two LOCAL ones the newer wins
    /// rather than a fixed rank. A reading the proxy took off a response is
    /// bound to the account that served the turn; a transcript is bound to the
    /// home. Both are honest, and the stale one is simply older.
    #[test]
    fn between_two_local_readings_the_newer_one_answers() {
        let w = |pct: f64| crate::codex_limits::Window {
            used_pct: pct,
            window_minutes: 10080,
            resets_at: Some(1_787_011_538),
        };
        let transcript = crate::codex_limits::Limits {
            short: Some(w(12.0)),
            long: None,
            observed_at: Some(1_786_500_000),
        };
        let seen_by_proxy = crate::quota_cache::Entry {
            seven_d: Some(55.0),
            seven_d_reset: Some(1_787_011_538),
            at: 1_786_590_000,
            ..Default::default()
        };
        let now = 1_786_600_000;

        // The proxy saw it more recently than the transcript was written.
        let (_, u) = codex_row(
            "work",
            None,
            None,
            Some(seen_by_proxy.clone()),
            Some(transcript),
            now,
        )
        .expect("a reading");
        assert_eq!(u.seven_d, Some(55.0));
        assert_eq!(u.observed_at, Some(1_786_590_000));

        // ... and when the transcript is the newer of the two, it answers.
        let older_proxy = crate::quota_cache::Entry {
            at: 1_786_400_000,
            ..seen_by_proxy.clone()
        };
        let (_, u) = codex_row(
            "work",
            None,
            None,
            Some(older_proxy.clone()),
            Some(transcript),
            now,
        )
        .expect("a reading");
        assert_eq!(u.seven_d, Some(12.0));

        // A proxy reading alone still answers - this is the source that works
        // for a home holding no transcripts while the machine is offline.
        let (_, u) = codex_row("work", None, None, Some(seen_by_proxy.clone()), None, now)
            .expect("a reading");
        assert_eq!(u.seven_d, Some(55.0));

        // The live endpoint outranks both, being taken just now.
        let live = crate::codex_usage::Account {
            limits: crate::codex_limits::Limits {
                short: Some(w(84.0)),
                long: None,
                observed_at: None,
            },
            ..Default::default()
        };
        let (_, u) = codex_row(
            "work",
            Some(&live),
            None,
            Some(seen_by_proxy.clone()),
            Some(transcript),
            now,
        )
        .expect("a reading");
        assert_eq!(u.seven_d, Some(84.0));
        assert_eq!(u.observed_at, Some(now));
    }

    /// A live answer from the account beats a transcript, and the transcript
    /// still answers when there is no live one. An account with neither is
    /// absent rather than shown at zero - zero is a number, and it would read
    /// as a full account sitting idle.
    #[test]
    fn a_live_reading_wins_over_a_transcript_and_a_transcript_over_nothing() {
        let w = |pct: f64| crate::codex_limits::Window {
            used_pct: pct,
            window_minutes: 10080,
            resets_at: Some(1_787_011_538),
        };
        let live = crate::codex_usage::Account {
            email: Some("someone@example.com".into()),
            limits: crate::codex_limits::Limits {
                short: Some(w(84.0)),
                long: None,
                observed_at: None,
            },
            ..Default::default()
        };
        let stale = crate::codex_limits::Limits {
            short: Some(w(12.0)),
            long: None,
            observed_at: Some(1_786_000_000),
        };
        let now = 1_786_600_000;

        let (who, u) =
            codex_row("work", Some(&live), None, None, Some(stale), now).expect("live answers");
        assert_eq!(who, "work");
        assert_eq!(u.seven_d, Some(84.0));
        // A live answer is stamped now, not left to inherit the transcript age.
        assert_eq!(u.observed_at, Some(now));

        let (_, u) =
            codex_row("work", None, None, None, Some(stale), now).expect("transcript answers");
        assert_eq!(u.seven_d, Some(12.0));
        assert_eq!(u.observed_at, Some(1_786_000_000));

        // The case this whole change exists for: a home with no transcripts at
        // all, whose account can still answer for itself.
        let (_, u) =
            codex_row("work", Some(&live), None, None, None, now).expect("live alone answers");
        assert_eq!(u.seven_d, Some(84.0));

        assert!(codex_row("work", None, None, None, None, now).is_none());
    }

    /// A Codex reading is keyed by the home it was read from, and by nothing
    /// else. The previous version captioned it with whoever the switch timeline
    /// said was paying, which put a reading on an account holding no transcripts
    /// while the one holding every transcript showed none. Passing the home as the
    /// only argument is what makes that impossible to reintroduce here. A live
    /// reading from `codex_usage` names its own account and needs no such care;
    /// it comes through here only to share one shape with the transcript path.
    #[test]
    fn a_codex_reading_is_keyed_by_the_home_it_came_from() {
        let l = crate::codex_limits::Limits {
            short: None,
            long: Some(crate::codex_limits::Window {
                used_pct: 45.0,
                window_minutes: 10080,
                resets_at: Some(1_787_011_538),
            }),
            observed_at: Some(1_786_600_000),
        };
        let (who, u) = codex_usage_row("codex-main", &l);
        assert_eq!(who, "codex-main");
        // A weekly-only reading fills the weekly column and leaves 5h empty,
        // rather than being forced into the session slot.
        assert_eq!(u.seven_d, Some(45.0));
        assert_eq!(u.five_h, None);
        assert_eq!(u.seven_d_reset, Some(1_787_011_538));
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

    // Device-code login policy: on by default, SWAPDEX_CODEX_LOGIN=browser opts
    // out. (That the flag actually reaches `codex login` is the argv test below.)
    #[test]
    fn device_auth_policy() {
        use super::codex_device_auth;
        assert!(codex_device_auth(false), "device-auth by default");
        assert!(!codex_device_auth(true), "opt-out -> browser flow");
    }

    // #4: a restore whose apply FAILS must not strand the requested backup. The
    // outgoing login is backed up only AFTER apply succeeds; otherwise it becomes
    // the newest backup and a retry sees it as "already active", never restoring
    // the target we were asked for.
    #[test]
    fn failed_restore_keeps_the_requested_backup_newest() {
        use crate::adapters::claude::Claude;
        use crate::adapters::AuthTool;
        use crate::paths::Paths;
        use crate::store::Store;

        fn seed(p: &Paths, uuid: &str, email: &str) {
            std::fs::create_dir_all(p.claude_credentials().parent().unwrap()).unwrap();
            std::fs::write(
                p.claude_credentials(),
                serde_json::to_vec(&serde_json::json!({"claudeAiOauth": {
                    "accessToken": "AT", "refreshToken": "RT", "expiresAt": 9999999999999i64,
                    "scopes": ["x"], "subscriptionType": "max", "rateLimitTier": "default"}}))
                .unwrap(),
            )
            .unwrap();
            std::fs::write(
                p.claude_config_json(),
                serde_json::to_vec(&serde_json::json!({
                    "oauthAccount": {"accountUuid": uuid, "emailAddress": email}}))
                .unwrap(),
            )
            .unwrap();
        }

        // Backup A (the target) lives in the LIVE machine's store.
        let liveroot = tempfile::tempdir().unwrap();
        let plive = Paths::rooted(liveroot.path());
        let aroot = tempfile::tempdir().unwrap();
        let pa = Paths::rooted(aroot.path());
        seed(&pa, "uuid-A", "a@x.com");
        let snap_a = Claude.capture(&pa).unwrap();
        let store = Store::open(&plive).unwrap();
        store.backup(&snap_a).unwrap();

        // The live login is B (a different account).
        seed(&plive, "uuid-B", "b@y.com");

        // Force apply(A) to fail. Planting a directory at the atomic temp path
        // used to work, but that path now carries a pid and a counter so two
        // writers cannot collide - a test that knows the temp name is testing
        // the name. Make the DESTINATION unwritable, which is the condition
        // this test is actually about.
        let cfg = plive.claude_config_json();
        std::fs::remove_file(&cfg).ok();
        std::fs::create_dir(&cfg).unwrap();

        assert!(
            super::restore(&plive, None, false).is_err(),
            "apply must fail (planted temp dir)"
        );

        // A must still be the NEWEST backup: B was never promoted, so a retry
        // restores A instead of hitting "already active".
        let (_stamp, newest) = store.load_backup("claude-code").unwrap().unwrap();
        assert_eq!(
            super::snapshot_account_id(&newest, "claude-code").as_deref(),
            Some("uuid-A"),
            "a failed restore must not strand A by making B the newest backup"
        );
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

#[cfg(test)]
mod short_absence_tests {
    use super::*;

    /// An empty short status must say which kind of nothing it is.
    ///
    /// `short_line` returns None both when no tool is signed in and when the
    /// logins cannot be read from this shell, and the caller printed a blank
    /// line for both. A locked macOS Keychain over SSH produces the second, and
    /// reading that as the first is exactly how a working account gets reported
    /// as signed out.
    #[test]
    fn an_empty_short_status_distinguishes_unreadable_from_signed_out() {
        assert_eq!(absence_reason(true), "logins unreadable from this shell");
        assert_eq!(absence_reason(false), "not signed in to any tool");
    }
}

#[cfg(test)]
mod bar_age_tests {
    use super::*;

    /// The status bar must not present an old number as the current one.
    ///
    /// The bar reads the cache, which carries the moment each number was taken,
    /// and threw that away. An account read before its window filled showed
    /// "5h 100%" for hours - the same shape as the reading that made a spent
    /// account look fine. The bar refreshes about once a minute, so anything
    /// past ten minutes means the readings stopped arriving, which is the one
    /// thing worth a few characters.
    #[test]
    fn the_bar_marks_a_number_that_stopped_being_refreshed() {
        // An account with plenty left is re-read every 15 minutes BY DESIGN, so
        // a flat ten-minute threshold flagged it during normal operation - a
        // warning that appears when nothing is wrong is one the reader learns
        // to skip. Late means late for THIS account's own schedule.
        let roomy = 900; // >50% headroom: read every 15 minutes
        assert_eq!(
            bar_age(840, roomy),
            None,
            "14m old on a 15m schedule is fine"
        );
        assert_eq!(bar_age(1_799, roomy), None);
        assert_eq!(bar_age(1_800, roomy), Some(" · 30m old".to_string()));

        // A spent account is re-read every minute, but never nag before ten.
        let tight = 60;
        assert_eq!(bar_age(300, tight), None);
        assert_eq!(bar_age(599, tight), None);
        assert_eq!(bar_age(600, tight), Some(" · 10m old".to_string()));
        assert_eq!(bar_age(9_000, tight), Some(" · 2h old".to_string()));
    }
}

#[cfg(test)]
mod throttled_note_tests {
    use super::*;

    /// A throttled endpoint is not a verdict on the account - in either direction.
    ///
    /// `quota.rs` already guards this on the way in: a 429 is classified as the
    /// ENDPOINT being busy, with a test named for it. The line printed on the
    /// way out then said "the account is fine", which is the same mistake
    /// mirrored - avoiding a false alarm by making a false reassurance. A real
    /// account that was spent and refusing every turn showed exactly this line.
    #[test]
    fn a_throttled_read_claims_nothing_about_the_account() {
        let n = throttled_note();
        assert!(
            !n.contains("account is fine"),
            "a declined read is not evidence the account is healthy: {n}"
        );
        assert!(
            n.contains("no reading"),
            "say what actually happened - there is no reading: {n}"
        );
        assert!(
            n.contains("try again"),
            "the remedy still belongs here: {n}"
        );
    }
}

#[cfg(test)]
mod install_verdict_tests {
    use super::*;

    /// Installing a service that cannot start is not a successful install.
    ///
    /// `service install` wrote the unit and announced "it starts at login, comes
    /// back if it stops" without ever checking that it had. On a Mac whose
    /// launchd context cannot open the Keychain, the proxy refuses to run - by
    /// design, because it would forward the user's own login and never say so -
    /// so the unit failed every start while swapdex reported it as installed.
    /// The only proxy on that machine was one somebody had started by hand,
    /// which is why it also never picked up an upgrade.
    #[test]
    fn an_install_that_never_came_up_is_reported_as_a_failure() {
        let (ok, msg) = install_verdict(true, "claude-code");
        assert!(ok, "it came up: {msg}");
        assert!(msg.contains("running"), "say it is running: {msg}");

        let (ok, msg) = install_verdict(false, "claude-code");
        assert!(
            !ok,
            "a service that did not start is not installed successfully"
        );
        assert!(
            msg.contains("did not start"),
            "say plainly that it did not start: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("keychain"),
            "name the usual cause on this platform: {msg}"
        );
        assert!(
            msg.contains("swapdex proxy --ensure"),
            "give the way that does work: {msg}"
        );
    }
}

#[cfg(test)]
mod serving_reach_tests {
    use super::*;

    /// A tool with accounts but no proxy must say switching cannot reach a
    /// running session.
    ///
    /// swapdex reported the Codex login, listed three Codex accounts, and never
    /// mentioned that nothing was carrying Codex traffic - no proxy, no pinned
    /// address. A session opened believing swapdex was "on" for it was talking
    /// straight to the vendor, and switching accounts changed nothing it could
    /// see. Not a fault, but a fact the tool alone knows and did not say.
    #[test]
    fn a_tool_with_no_proxy_says_what_switching_can_and_cannot_do() {
        let served = serving_reach(true, 3).expect("a tool with accounts has something to say");
        assert!(
            served.contains("running session"),
            "say a running session can be moved: {served}"
        );

        let unserved = serving_reach(false, 3).unwrap();
        assert!(
            unserved.to_lowercase().contains("next session"),
            "say when a switch takes effect: {unserved}"
        );
        assert!(
            !unserved.to_lowercase().contains("problem"),
            "not having a proxy is a choice, not a fault: {unserved}"
        );

        // Nothing to say about a tool with no accounts at all.
        assert_eq!(serving_reach(false, 0), None);
    }
}
