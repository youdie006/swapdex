use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

// A fake `claude` that prints the CLAUDE_CONFIG_DIR it was launched with, then
// prints any args. `swapdex run` exec's it, so its stdout is what we capture.
fn fake_claude(root: &Path) -> std::path::PathBuf {
    let dir = root.join("fakebin");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("claude");
    std::fs::write(
        &f,
        "#!/bin/sh\necho \"CFG=$CLAUDE_CONFIG_DIR\"\necho \"ARGS=$*\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

#[test]
fn run_launches_claude_in_the_accounts_slot() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin())
        .args(["run", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    // The slot dir was created under the store and passed as CLAUDE_CONFIG_DIR.
    let slots = root.path().join(".local/share/swapdex/slots");
    assert!(
        o.lines()
            .any(|l| l.starts_with("CFG=") && l.contains(slots.to_str().unwrap())),
        "claude launched with the slot as CLAUDE_CONFIG_DIR: {o}"
    );
}

#[test]
fn run_forwards_extra_args_after_dash_dash() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin())
        .args(["run", "work", "--", "--resume", "abc"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(
        o.lines()
            .any(|l| l.starts_with("ARGS=") && l.contains("--resume abc")),
        "extra args are forwarded to claude: {o}"
    );
}

#[test]
fn slots_lists_created_slots() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // `run` creates the slot; then `slots` should list it.
    Command::new(bin())
        .args(["run", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let out = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(o.contains("work"), "the slot is listed: {o}");
}

#[test]
fn slots_empty_state_is_friendly() {
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    // "Slot" is the internal name for an account's own space; a person reading
    // the screen should only ever see "account".
    assert!(
        o.to_lowercase().contains("no accounts yet"),
        "empty-state hint: {o}"
    );
    assert!(!o.to_lowercase().contains("slot"), "no jargon: {o}");
}

fn run_in(root: &Path, args: &[&str], path_env: &str) -> String {
    // HOME must point inside the temp root. `swapdex shim` offers to put itself
    // on PATH by editing the shell profile of $HOME, and without this a test run
    // appended an export line - naming a temp dir that is deleted moments later -
    // to the developer's own ~/.bashrc.
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("PATH", path_env)
        .env("HOME", &home)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// End-to-end: `run` makes a slot, `use` repoints the default (no copy), the
// installed shim launches a plain `claude` in that default slot.
#[test]
fn shim_makes_plain_claude_follow_use() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // Create the slot and set it as the default account.
    run_in(root.path(), &["run", "work"], &path);
    let used = run_in(root.path(), &["use", "work"], &path);
    assert!(
        // The tool is named now that Claude and Codex both switch by pointer.
        used.contains("default claude account -> work"),
        "use repoints: {used}"
    );
    // Install the shim (finds the fake claude on PATH as the real one).
    let installed = run_in(root.path(), &["shim"], &path);
    assert!(
        installed.contains("installed the claude shim"),
        "{installed}"
    );
    // Run the shim directly; it should exec the fake claude with the slot dir.
    // With the test's own environment: the shim asks swapdex for a proxy, and
    // without SWAPDEX_ROOT that question is asked of the DEVELOPER's real store -
    // which answered by starting a daemon against their real accounts, on the
    // default port, outliving the test.
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let shim = root.path().join(".local/share/swapdex/bin/claude");
    let out = Command::new(&shim)
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .env("HOME", &home)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    let slots = root.path().join(".local/share/swapdex/slots");
    assert!(
        o.lines()
            .any(|l| l.starts_with("CFG=") && l.contains(slots.to_str().unwrap())),
        "the shim launched claude in the default account's slot: {o}"
    );
    // Stop whatever proxy the shim started for this temp store.
    if let Ok(marker) = std::fs::read_to_string(root.path().join(".local/share/swapdex/proxy")) {
        if let Some(pid) = marker
            .split_whitespace()
            .next()
            .and_then(|p| p.parse::<i32>().ok())
        {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

#[test]
fn use_on_a_slot_does_not_touch_the_copy_model_credentials() {
    // A slot `use` must not read/write ~/.claude - it only sets the pointer.
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(root.path(), &["run", "work"], &path);
    run_in(root.path(), &["use", "work"], &path);
    // No live Claude credential file was created by the switch.
    assert!(
        !root.path().join(".claude/.credentials.json").exists(),
        "slot use writes no credential"
    );
    // The pointer holds the slot dir.
    let ptr =
        std::fs::read_to_string(root.path().join(".local/share/swapdex/active-claude")).unwrap();
    assert!(ptr.contains("/slots/"), "pointer points at a slot: {ptr}");
}

#[test]
fn adopt_registers_an_existing_config_dir() {
    let root = tempfile::tempdir().unwrap();
    let existing = root.path().join("dot-claude-company");
    std::fs::create_dir_all(&existing).unwrap();
    let out = Command::new(bin())
        .args(["adopt", "company", existing.to_str().unwrap()])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(o.contains("registered 'company'"), "{o}");
    // It now shows up in the slot list.
    let listed = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("company"));
}

// Seed a legacy copy-model Claude profile named `name` in the store.
fn seed_copy_profile(root: &Path, name: &str) {
    let d = root
        .join(".local/share/swapdex/accounts")
        .join(name)
        .join("claude-code");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("credentials"),
        br#"{"claudeAiOauth":{"accessToken":"A","refreshToken":"R"}}"#,
    )
    .unwrap();
    std::fs::write(
        d.join("oauth_account"),
        br#"{"accountUuid":"u","emailAddress":"a@x.com"}"#,
    )
    .unwrap();
}

#[test]
fn migrate_gives_each_legacy_profile_a_slot() {
    let root = tempfile::tempdir().unwrap();
    seed_copy_profile(root.path(), "work");
    seed_copy_profile(root.path(), "home");
    let out = Command::new(bin())
        .args(["migrate"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(
        o.contains("work") && o.contains("home"),
        "migrated both: {o}"
    );
    // Both are now slots.
    let listed = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let l = String::from_utf8_lossy(&listed.stdout);
    assert!(
        l.contains("work") && l.contains("home"),
        "listed as slots: {l}"
    );
    // Re-running is idempotent (nothing left to migrate).
    let again = Command::new(bin())
        .args(["migrate"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&again.stdout)
            .to_lowercase()
            .contains("nothing to migrate"),
        "idempotent"
    );
}

/// A legacy profile carrying a name no creator will accept must not take the
/// rest of the migration down with it.
///
/// `migrate` already renames a profile whose name a creator would refuse - a
/// tool-home name, or one a slot already holds - via `suggest_non_colliding`.
/// `-` is refused for the same kind of reason (`use -` toggles), and it is
/// neither of those two, so it reached `create_initialized` and the `?` there
/// aborted the whole command: every profile after it in the sweep was skipped.
#[test]
fn migrate_renames_a_legacy_profile_whose_name_no_creator_accepts() {
    let root = tempfile::tempdir().unwrap();
    // "-" sorts before "work", so an abort here loses "work" too.
    seed_copy_profile(root.path(), "-");
    seed_copy_profile(root.path(), "work");
    let out = Command::new(bin())
        .args(["migrate"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    let e = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "migrate failed: {o}{e}");

    // The profile after the bad name still got its slot.
    let listed = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let l = String::from_utf8_lossy(&listed.stdout);
    assert!(l.contains("work"), "later profile was skipped: {l}");

    // And the refused name was renamed, not registered.
    let rows: Vec<serde_json::Value> =
        std::fs::read(root.path().join(".local/share/swapdex/slots.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
    assert!(
        rows.iter().any(|r| r["name"] == "account"),
        "the refused name was not renamed: {rows:?}"
    );
}

/// The sign-in line migrate prints has to be a line that runs.
///
/// Every remedy this tool prints - about thirty of them - puts the slot name
/// where clap expects a positional, so a name beginning with `-` is read there
/// as an option and the command exits 2 before it reaches the account. Nothing
/// refused such a name where an account is born, so migrate registered it and
/// then handed out a command that cannot work.
#[test]
fn the_sign_in_line_migrate_prints_can_be_run() {
    let root = tempfile::tempdir().unwrap();
    seed_copy_profile(root.path(), "-x");
    let out = Command::new(bin())
        .args(["migrate"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    let e = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "migrate failed: {o}{e}");

    let line = o
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("swapdex run "))
        .unwrap_or_else(|| panic!("migrate printed no sign-in line: {o}"));
    let args: Vec<&str> = line.split_whitespace().skip(1).collect();

    let ran = Command::new(bin())
        .args(&args)
        .arg("--no-launch")
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(
        ran.status.code(),
        Some(0),
        "the line migrate printed does not run: `{line}`\n{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
}

#[test]
fn doctor_reports_slots_default_and_shim() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(root.path(), &["run", "work"], &path);
    run_in(root.path(), &["use", "work"], &path);
    let out = Command::new(bin())
        .args(["doctor"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(
        o.contains("slots") && o.contains("account(s)"),
        "slots line: {o}"
    );
    assert!(
        o.contains("default") && o.contains("work"),
        "default line: {o}"
    );
    assert!(o.contains("shim"), "shim line: {o}");
}

#[test]
fn new_slot_symlinks_shared_config_from_bare_claude() {
    let root = tempfile::tempdir().unwrap();
    // Bare ~/.claude with shared config the new slot should inherit.
    let bare = root.path().join(".claude");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(bare.join("settings.json"), b"{\"theme\":\"dark\"}").unwrap();
    std::fs::write(bare.join("CLAUDE.md"), b"# global rules").unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(root.path(), &["run", "work"], &path);
    // The slot got symlinks to the shared files (same contents), but NOT a token.
    let slots = root.path().join(".local/share/swapdex/slots");
    let slot = std::fs::read_dir(&slots)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_to_string(slot.join("settings.json")).unwrap(),
        "{\"theme\":\"dark\"}",
        "settings shared into the slot"
    );
    assert!(slot.join("CLAUDE.md").exists(), "global memory shared");
    assert!(
        std::fs::symlink_metadata(slot.join("settings.json"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "shared via symlink, not a copy"
    );
}

#[test]
fn onboard_registers_config_dirs_and_migrates_profiles() {
    let root = tempfile::tempdir().unwrap();
    // State 3: an existing ~/.claude-company dir. State 2: a legacy copy profile.
    std::fs::create_dir_all(root.path().join(".claude-company")).unwrap();
    seed_copy_profile(root.path(), "work");
    let out = Command::new(bin())
        .args(["onboard"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn_with_input("y\ny\nn\n");
    // Both an adopted slot and a migrated slot now exist.
    let listed = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let l = String::from_utf8_lossy(&listed.stdout);
    assert!(
        l.contains("company"),
        "adopted the config dir: {l} / onboard out: {out}"
    );
    assert!(l.contains("work"), "migrated the legacy profile: {l}");
}

// Small helper: run with piped stdin, return stdout.
trait SpawnWithInput {
    fn spawn_with_input(&mut self, input: &str) -> String;
}
impl SpawnWithInput for Command {
    fn spawn_with_input(&mut self, input: &str) -> String {
        use std::io::Write;
        let mut child = self.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

#[test]
fn onboard_marks_itself_done_so_it_does_not_nag() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".claude-company")).unwrap();
    // Before onboarding, the marker is absent (bare `swapdex` would offer it).
    let marker = root.path().join(".local/share/swapdex/onboarded");
    assert!(!marker.exists());
    // Run onboard (decline everything); it should still mark itself shown.
    Command::new(bin())
        .args(["onboard"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn_with_input("n\nn\n");
    assert!(marker.exists(), "onboarding marks itself done");
}

#[test]
fn sync_mcp_shares_servers_into_slots_preserving_identity() {
    let root = tempfile::tempdir().unwrap();
    // Source: bare ~/.claude.json with mcpServers.
    std::fs::write(
        root.path().join(".claude.json"),
        br#"{"oauthAccount":{"emailAddress":"bare@x.com"},"mcpServers":{"ctx7":{"command":"c"}}}"#,
    )
    .unwrap();
    // A slot that has already "logged in" (its own .claude.json with a different account, no MCP).
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(root.path(), &["run", "work"], &path);
    let slots = root.path().join(".local/share/swapdex/slots");
    let slot = std::fs::read_dir(&slots)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(
        slot.join(".claude.json"),
        br#"{"oauthAccount":{"emailAddress":"work@x.com"},"mcpServers":{}}"#,
    )
    .unwrap();
    // Sync: the slot gets the shared MCP but keeps its own account.
    let out = Command::new(bin())
        .args(["sync-mcp"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 MCP server"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let cfg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(slot.join(".claude.json")).unwrap()).unwrap();
    assert!(
        cfg["mcpServers"]["ctx7"].is_object(),
        "shared MCP landed in the slot"
    );
    assert_eq!(
        cfg["oauthAccount"]["emailAddress"], "work@x.com",
        "slot's own account preserved"
    );
}

// doctor: per-slot login health. A slot that was never signed into and a slot
// whose login sat unrefreshed past the stale window are each named with the
// one next step; a slot whose access token expired ROUTINELY (hours ago -
// Claude refreshes that silently on the next run) is NOT flagged, or doctor
// would cry "expired" every day. Read-only: doctor never writes a slot.
#[test]
fn doctor_flags_slots_without_login_and_stale_logins() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    for name in ["empty", "old", "recent", "corrupt"] {
        run_in(root.path(), &["run", name], &path);
    }
    // Slot dirs from the `slots` listing lines: "  <name>  <dir>".
    let listing = run_in(root.path(), &["slots"], &path);
    let dir_of = |name: &str| -> std::path::PathBuf {
        listing
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix(name)
                    .map(|rest| std::path::PathBuf::from(rest.trim()))
            })
            .unwrap_or_else(|| panic!("slot '{name}' in listing: {listing}"))
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let creds = |expires_ms: i64| {
        format!(
            "{{\"claudeAiOauth\":{{\"accessToken\":\"AT\",\"refreshToken\":\"RT\",\
             \"expiresAt\":{expires_ms}}}}}"
        )
    };
    // 'old': expired 40 days ago - the refresh token itself may be revoked.
    std::fs::write(
        dir_of("old").join(".credentials.json"),
        creds(now_ms - 40 * 86_400_000),
    )
    .unwrap();
    // 'recent': expired 2 hours ago - routine, silently refreshed on next run.
    std::fs::write(
        dir_of("recent").join(".credentials.json"),
        creds(now_ms - 2 * 3_600_000),
    )
    .unwrap();
    // 'corrupt': a login artifact EXISTS but is unparseable - not "no login".
    std::fs::write(dir_of("corrupt").join(".credentials.json"), b"not json").unwrap();
    // PATH holds ONLY the fake tool dir. doctor now reports how many swapdex
    // copies are reachable, and inheriting the developer's PATH would make that
    // answer - and this test - depend on whose machine it runs on.
    let out = Command::new(bin())
        .args(["doctor"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", bin_dir.display().to_string())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "informational, stays exit 0: {o}");
    assert!(
        o.contains("slot:empty") && o.contains("swapdex run empty"),
        "never-signed-in slot named with the run remedy: {o}"
    );
    assert!(
        o.contains("slot:old") && o.contains("swapdex run old"),
        "long-idle slot named with the run remedy: {o}"
    );
    assert!(
        !o.contains("slot:recent"),
        "routinely-expired slot is not flagged: {o}"
    );
    assert!(
        !o.contains("slot:corrupt"),
        "a PRESENT but unparseable credential is not 'no login yet' - doctor \
         only flags what it can determine: {o}"
    );
}

// doctor: an installed shim that PATH never reaches is a trap - it LOOKS set
// up while a plain `claude` still runs bare, so `swapdex use` silently does
// nothing (the pointer flips but nothing reads it). doctor must say the shim
// is not taking effect (with the PATH fix), and call it active only when the
// shim really is what a plain `claude` resolves to.
#[test]
fn doctor_detects_shim_bypassed_and_active() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(root.path(), &["run", "work"], &path);
    run_in(root.path(), &["shim"], &path);
    // Shim dir NOT on PATH, but `swapdex shim` just edited the shell profile:
    // the setup IS right, this shell simply predates the edit. Saying "add it to
    // PATH" here sends someone to do what was already done, so doctor names the
    // profile and says to open a new terminal - and does NOT count it a problem.
    let out = run_in(root.path(), &["doctor"], &path);
    assert!(
        out.contains("not on THIS shell's PATH") && out.contains("open a new terminal"),
        "a shell predating the profile edit is not a broken setup: {out}"
    );
    assert!(
        !out.contains("shim          problem"),
        "and it is not counted as a fault: {out}"
    );

    // Nothing configuring it anywhere IS a real finding, with the PATH fix.
    let profile = root.path().join("home/.bashrc");
    if profile.exists() {
        std::fs::write(&profile, "").unwrap();
    }
    let out = run_in(root.path(), &["doctor"], &path);
    assert!(
        out.contains("NOT taking effect") && out.contains("PATH"),
        "an unconfigured shim is still called out with the PATH fix: {out}"
    );
    // Shim dir FIRST on PATH: the shim genuinely intercepts a plain `claude`.
    let shim_first = format!(
        "{}:{}",
        root.path().join(".local/share/swapdex/bin").display(),
        path
    );
    let out = run_in(root.path(), &["doctor"], &shim_first);
    assert!(
        out.contains("shim active"),
        "engaged shim reported active: {out}"
    );
}

// `swapdex auto` is the setting proxy mode reads, so it must persist and be
// readable back - and reject anything that is not on/off rather than guessing.
#[test]
fn auto_setting_round_trips_and_rejects_nonsense() {
    let root = tempfile::tempdir().unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    assert!(
        run_in(root.path(), &["auto"], &path).contains("off"),
        "off until asked for"
    );
    assert!(run_in(root.path(), &["auto", "on"], &path).contains("on"));
    assert!(
        run_in(root.path(), &["auto"], &path).contains("on"),
        "the setting persisted"
    );
    assert!(run_in(root.path(), &["auto", "off"], &path).contains("off"));
    let out = Command::new(bin())
        .args(["auto", "sometimes"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "a bad value is refused");
}

/// `--names` could not see a slot account, which `use` accepts.
///
/// The docs call it the form "for scripts and completion". It printed
/// `store.list()` and returned before the block that merges slot accounts into
/// the listing - so the human `ls` showed the account, `use <name>` switched to
/// it, and the one form written for tab-completion left it out. Every account
/// made by `swapdex run <name>` is exactly that shape.
#[test]
fn names_lists_the_accounts_use_accepts() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("slotdir");
    std::fs::create_dir_all(&dir).unwrap();
    seed_copy_profile(root.path(), "saved");
    let path = std::env::var("PATH").unwrap_or_default();
    run_in(
        root.path(),
        &["adopt", "slotonly", dir.to_str().unwrap()],
        &path,
    );

    // Precondition: it is a real account - `use` takes it, an invented name is 5.
    let used = Command::new(bin())
        .args(["use", "slotonly"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(used.status.code(), Some(0), "the slot account is usable");

    let names = run_in(root.path(), &["ls", "--names"], &path);
    assert!(names.contains("saved"), "the profile: {names}");
    assert!(
        names.lines().any(|l| l.trim() == "slotonly"),
        "and the slot account completion would need: {names}"
    );
}

/// `status --json` called a healthy login expired, nearly always.
///
/// A Claude access token lives about an hour and the tool refreshes it silently.
/// Two paths learned that - `ls`'s marker ("this was the constant 'expired'
/// spam") and `status`'s own note - and both only speak past STALE_DAYS. The
/// JSON kept the literal `expires_at < now`, so for most of every hour it
/// reported `"expired": true` about an account that was working, while the
/// human line right beside it said nothing was wrong.
#[test]
fn the_json_status_does_not_call_an_hourly_refresh_expired() {
    let root = tempfile::tempdir().unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::fs::create_dir_all(root.path().join(".claude")).unwrap();
    // The ordinary state: lapsed an hour ago, refreshes by itself.
    std::fs::write(
        root.path().join(".claude/.credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"T","expiresAt":{},"subscriptionType":"max"}}}}"#,
            now_ms - 3_600_000
        ),
    )
    .unwrap();
    std::fs::write(
        root.path().join(".claude.json"),
        br#"{"oauthAccount":{"accountUuid":"u-1","emailAddress":"a@x.com"}}"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["status", "--json"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("status --json is JSON");
    let row = v
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool"] == "claude-code")
        .expect("the claude row");
    assert_eq!(row["logged_in"], serde_json::json!(true), "{row}");
    assert_eq!(
        row["expired"],
        serde_json::json!(false),
        "an hourly refresh is not an expired login: {row}"
    );
}

/// The JSON listing dropped the warning that it was incomplete.
///
/// A damaged `slots.json` means slot accounts are missing from the listing, and
/// `ls` says so. `ls --json` returned before that line ever ran: a truncated
/// array, exit 0, nothing on stderr - and every row carrying `"warning": null`,
/// so the one field a consumer would check for trouble reported none. The
/// status line and any script read this form.
#[test]
fn the_json_listing_says_when_it_is_incomplete() {
    let root = tempfile::tempdir().unwrap();
    seed_copy_profile(root.path(), "work");
    let store = root.path().join(".local/share/swapdex");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("slots.json"), b"{ not json").unwrap();

    let out = Command::new(bin())
        .args(["ls", "--json"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // stdout stays parseable: a consumer mid-pipeline must not be handed prose.
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is still JSON ({e}): {stdout}"));
    assert!(parsed.is_array(), "the shape does not change: {stdout}");

    assert!(
        stderr.contains("could not be read") || stderr.contains("damaged"),
        "and the incompleteness is said out loud: stderr={stderr:?}"
    );
}

/// `rm --tool claude` could not drop a Claude login.
///
/// Seven commands take `--tool` as the parsed `ToolSel`; `rm` alone took a raw
/// `Option<String>` and handed it straight to `drop_tool`, which looks under
/// `accounts/<name>/<tool>`. The directory is `claude-code`, and `ToolSel`'s own
/// definition makes `claude` the canonical spelling with `claude-code` the alias,
/// which `docs/COMMANDS.md` states too. So the documented spelling was the one
/// that failed, and it failed by saying the profile "has no claude login" about
/// a profile whose listing shows exactly that login. The unparsed string also
/// let any word through: `--tool banana` answered "has no banana login" where
/// every other command answers "invalid value".
#[test]
fn rm_drops_the_tool_the_documented_flag_names() {
    let root = tempfile::tempdir().unwrap();
    seed_copy_profile(root.path(), "work");
    let path = std::env::var("PATH").unwrap_or_default();
    assert!(
        run_in(root.path(), &["ls"], &path).contains("claude-code"),
        "precondition: the profile has that login"
    );

    let out = Command::new(bin())
        .args(["rm", "work", "--tool", "claude"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(
        !run_in(root.path(), &["ls"], &path).contains("claude-code"),
        "the login is gone"
    );

    // And a word that is not a tool is a usage error, as everywhere else.
    let bad = Command::new(bin())
        .args(["rm", "work", "--tool", "banana"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2), "invalid value is exit 2");
}

/// A refused rename that already happened to half the account.
///
/// A name can be a registered slot AND a saved profile. `rename` renames the
/// slot first and persists it, THEN checks whether a profile already claims the
/// new name - and refuses with exit 6. So the slot is left renamed under a
/// message saying the rename did not happen: the mirror of `rm`, which did half
/// the job and called it success. Nothing may move unless all of it can.
#[test]
fn a_rename_that_is_refused_leaves_the_slot_alone() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("dir-a");
    std::fs::create_dir_all(&dir).unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    run_in(
        root.path(),
        &["adopt", "bsgong", dir.to_str().unwrap()],
        &path,
    );
    seed_copy_profile(root.path(), "bsgong");
    seed_copy_profile(root.path(), "kong");

    let out = Command::new(bin())
        .args(["rename", "bsgong", "kong"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6), "the target name is taken");

    // ...and the slot still answers to its old name.
    let slots = run_in(root.path(), &["slots"], &path);
    assert!(
        slots.contains("bsgong"),
        "a refused rename moved nothing: {slots}"
    );
    assert!(
        !slots.contains("kong"),
        "the slot must not carry the name the rename was refused: {slots}"
    );
}

/// One name, two halves - and `rm` says only what it did to the first.
///
/// A name can be a registered slot AND a saved profile at once. `rm` unregisters
/// the slot and deliberately leaves the profile ("a profile of the same name is a
/// separate thing"), then prints a plain success. On a real machine the account
/// was still listed afterwards, so the removal read as failed, and the next
/// `swapdex add <name>` refused with "already has a claude-code login" - which is
/// where the user ends up: told it exists after being told it was removed.
#[test]
fn rm_says_when_a_saved_profile_of_the_same_name_is_still_there() {
    let root = tempfile::tempdir().unwrap();
    let existing = root.path().join("dot-claude-kong");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join(".credentials.json"), b"{}").unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    run_in(
        root.path(),
        &["adopt", "kong", existing.to_str().unwrap()],
        &path,
    );
    seed_copy_profile(root.path(), "kong");

    let out = run_in(root.path(), &["rm", "kong", "--yes"], &path);
    assert!(out.contains("stopped managing"), "the slot went: {out}");
    assert!(
        out.contains("saved profile"),
        "and it says the other half is still here: {out}"
    );
    assert!(
        out.contains("rm kong"),
        "naming the command that removes it: {out}"
    );
    // The listing still shows the name, which is the whole reason this has to
    // be said out loud.
    assert!(
        run_in(root.path(), &["ls"], &path).contains("kong"),
        "the profile is still listed"
    );
}

/// Removing a slot account means "stop managing it", never "lose it": the mapping
/// goes, the directory and the login inside it stay, and `adopt` can bring it back.
#[test]
fn rm_unregisters_a_slot_and_leaves_its_login_alone() {
    let root = tempfile::tempdir().unwrap();
    let existing = root.path().join("dot-claude-company");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join(".credentials.json"), b"{\"keep\":\"me\"}").unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    run_in(
        root.path(),
        &["adopt", "company", existing.to_str().unwrap()],
        &path,
    );
    assert!(run_in(root.path(), &["slots"], &path).contains("company"));

    let out = run_in(root.path(), &["rm", "company", "--yes"], &path);
    assert!(out.contains("stopped managing"), "{out}");
    // ...and does not invent a leftover: nothing named 'company' was ever saved,
    // so pointing at a saved profile would send the user after a thing that is
    // not there.
    assert!(
        !out.contains("saved profile"),
        "no snapshot exists, so none is claimed: {out}"
    );
    assert!(
        !run_in(root.path(), &["slots"], &path).contains("company"),
        "the mapping is gone"
    );
    assert!(
        existing.join(".credentials.json").exists(),
        "the login was never touched"
    );
    // And it can be brought back.
    run_in(
        root.path(),
        &["adopt", "company", existing.to_str().unwrap()],
        &path,
    );
    assert!(run_in(root.path(), &["slots"], &path).contains("company"));
}

/// The shim is useless if PATH never reaches it, so installing it edits the shell
/// profile - once, idempotently, and never for a shell we would only be guessing
/// about.
#[test]
fn shim_puts_itself_on_path_via_the_shell_profile() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let run_shim = || {
        String::from_utf8_lossy(
            &Command::new(bin())
                .args(["shim"])
                .env("SWAPDEX_ROOT", root.path())
                .env("PATH", &path)
                .env("HOME", &home)
                .env("SHELL", "/bin/zsh")
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned()
    };

    let out = run_shim();
    assert!(
        out.contains("added it to"),
        "it says what it changed: {out}"
    );
    let zshrc = std::fs::read_to_string(home.join(".zshrc")).expect("profile written");
    assert!(
        zshrc.contains("swapdex") && zshrc.contains("export PATH="),
        "the line is there: {zshrc}"
    );

    // Running it again must not stack a second copy.
    run_shim();
    let again = std::fs::read_to_string(home.join(".zshrc")).unwrap();
    assert_eq!(
        again.matches("export PATH=").count(),
        1,
        "idempotent: {again}"
    );

    // A shell we cannot reason about is told, not edited.
    let out = String::from_utf8_lossy(
        &Command::new(bin())
            .args(["shim"])
            .env("SWAPDEX_ROOT", root.path())
            .env("PATH", &path)
            .env("HOME", &home)
            .env("SHELL", "/usr/bin/fish")
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();
    assert!(
        out.contains("add this to your shell profile"),
        "fish gets instructions rather than a guessed edit: {out}"
    );
    assert!(
        !home.join(".config").exists(),
        "nothing was written for a shell we do not handle"
    );
}

/// Switching should not mean leaving the conversation for another terminal, so
/// swapdex installs a Claude Code slash command that does it in place.
#[test]
fn slash_installs_a_claude_code_command() {
    let root = tempfile::tempdir().unwrap();
    // Under `SWAPDEX_ROOT` the root IS the home - that is what `Paths::rooted`
    // means and what containment is. `slash` used to ask `dirs` for the home
    // instead, so it honoured $HOME here and the REAL one everywhere else.
    let home = root.path().to_path_buf();
    let out = String::from_utf8_lossy(
        &Command::new(bin())
            .args(["slash"])
            .env("SWAPDEX_ROOT", root.path())
            .env("HOME", &home)
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();
    assert!(out.contains("/swap"), "it names the command: {out}");

    // Each assistant only ever sees - and only ever moves - its OWN accounts.
    let claude_body = std::fs::read_to_string(home.join(".claude/commands/swap.md"))
        .expect("claude command written");
    assert!(claude_body.starts_with("---"), "frontmatter: {claude_body}");
    assert!(
        claude_body.contains("--tool claude-code") && !claude_body.contains("--tool codex"),
        "the Claude command switches Claude only: {claude_body}"
    );
    assert!(
        claude_body.contains("tagged `claude-code`"),
        "and lists Claude accounts only: {claude_body}"
    );

    let codex_body =
        std::fs::read_to_string(home.join(".codex/skills/swap/SKILL.md")).expect("codex skill");
    assert!(
        codex_body.contains("name: swap") && codex_body.contains("description:"),
        "codex frontmatter: {codex_body}"
    );
    assert!(
        codex_body.contains("--tool codex") && !codex_body.contains("--tool claude-code"),
        "the Codex skill switches Codex only: {codex_body}"
    );
    assert!(
        codex_body.contains("AskUserQuestion"),
        "a bare /swap still offers a pick-list: {codex_body}"
    );
    let f = home.join(".claude/commands/swap.md");
    let body = claude_body;

    // Re-running just rewrites it - no duplicate, no error.
    Command::new(bin())
        .args(["slash"])
        .env("SWAPDEX_ROOT", root.path())
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), body);
}

/// The threshold is a setting, not just a flag: the proxy the shim starts takes
/// no flags, and that is the one doing the work day to day.
#[test]
fn threshold_setting_accepts_both_notations_and_off() {
    let root = tempfile::tempdir().unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    assert!(
        run_in(root.path(), &["threshold"], &path).contains("no threshold"),
        "off until asked for"
    );
    // Fractions and percentages are both how people say this.
    assert!(run_in(root.path(), &["threshold", "0.9"], &path).contains("90%"));
    assert!(
        run_in(root.path(), &["threshold"], &path).contains("90%"),
        "it persists"
    );
    assert!(run_in(root.path(), &["threshold", "80%"], &path).contains("80%"));
    assert!(run_in(root.path(), &["threshold", "95"], &path).contains("95%"));
    // And it can be turned back off.
    assert!(run_in(root.path(), &["threshold", "off"], &path).contains("threshold off"));
    assert!(run_in(root.path(), &["threshold"], &path).contains("no threshold"));
    // Nonsense is refused rather than guessed at.
    let out = Command::new(bin())
        .args(["threshold", "soonish"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// A stand-in `codex` that reports the home it was launched with.
fn fake_codex(root: &Path) -> std::path::PathBuf {
    let dir = root.join("fakebin");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("codex");
    std::fs::write(
        &f,
        "#!/bin/sh\necho \"HOME_DIR=$CODEX_HOME\"\necho \"ARGS=$*\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

// `run` is how an account gets its login in the first place: it makes the slot
// and launches the tool pointed at it, so the sign-in lands in that account's own
// home instead of the shared one every other account also reads.
#[test]
fn run_launches_codex_in_the_accounts_own_home() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_codex(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = run_in(root.path(), &["run", "work", "--tool", "codex"], &path);
    let store = root.path().join(".local/share/swapdex");
    let recs: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(store.join("slots.json")).unwrap()).unwrap();
    let rec = recs
        .iter()
        .find(|r| r["name"] == "work")
        .expect("the slot was created");
    assert_eq!(rec["tool"], "codex", "registered as a Codex account");
    let dir = rec["config_dir"].as_str().unwrap();
    assert!(
        out.contains(&format!("HOME_DIR={dir}")),
        "codex was launched with that home: {out}"
    );
    // Claude's variable is not involved - one tool's launch must never point the
    // other tool anywhere.
    assert!(!out.contains("CLAUDE_CONFIG_DIR"), "{out}");
}

// Every account is a slot now, and the UI's rename only ever touched the store -
// so renaming from the dashboard failed with "no profile named X" on exactly the
// accounts the dashboard is made of.
#[test]
fn rename_works_on_a_slot_account() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(root.path(), &["run", "work", "--no-launch"], &path);
    let out = run_in(root.path(), &["rename", "work", "acme"], &path);
    assert!(out.contains("acme"), "the rename reported success: {out}");
    let listed = run_in(root.path(), &["slots"], &path);
    assert!(listed.contains("acme"), "the new name is listed: {listed}");
    assert!(!listed.contains("work"), "the old one is gone: {listed}");
}

// Every account in the dashboard is a slot, and both tools put them there. `rm`
// looked only in Claude's registry, so a Codex account could not be removed at
// all - and the dashboard's delete key reported "no profile named X" for the
// accounts it was showing.
#[test]
fn removing_an_account_works_for_either_tool() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(root.path(), &["run", "onclaude", "--no-launch"], &path);
    run_in(
        root.path(),
        &["run", "oncodex", "--tool", "codex", "--no-launch"],
        &path,
    );

    let out = run_in(root.path(), &["rm", "oncodex", "--yes"], &path);
    assert!(
        out.contains("oncodex"),
        "a Codex account can be removed: {out}"
    );
    let listed = run_in(root.path(), &["slots"], &path);
    assert!(!listed.contains("oncodex"), "it is gone: {listed}");
    assert!(
        listed.contains("onclaude"),
        "the other is untouched: {listed}"
    );

    // And Claude's still work.
    run_in(root.path(), &["rm", "onclaude", "--yes"], &path);
    let listed = run_in(root.path(), &["slots"], &path);
    assert!(!listed.contains("onclaude"), "{listed}");
}

// Which tool an account belongs to was worked out separately everywhere it was
// needed, and the versions disagreed: one fell back to Claude whenever the slot
// registry did not know the name, so pressing the sign-in key on a Codex account
// opened Claude's login.
#[test]
fn an_accounts_tool_is_read_the_same_way_everywhere() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(
        root.path(),
        &["run", "oncodex", "--tool", "codex", "--no-launch"],
        &path,
    );
    run_in(root.path(), &["run", "onclaude", "--no-launch"], &path);
    // Both slots are signed in: `serve` refuses an account that has no login,
    // because the proxy would forward the user's OWN credential and every screen
    // would still name this one. That guard is not what this test is about.
    let slot_of = |name: &str| -> std::path::PathBuf {
        let raw = std::fs::read_to_string(root.path().join(".local/share/swapdex/slots.json"))
            .expect("slots.json");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let dir = v
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap()["config_dir"]
            .as_str()
            .unwrap()
            .to_string();
        std::path::PathBuf::from(dir)
    };
    std::fs::write(
        slot_of("oncodex").join("auth.json"),
        br#"{"tokens":{"access_token":"a","account_id":"acc"}}"#,
    )
    .unwrap();
    std::fs::write(
        slot_of("onclaude").join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"a","expiresAt":32503680000000}}"#,
    )
    .unwrap();

    // `serve` is what the dashboard's Enter runs, and it is told the tool the
    // dashboard resolved - so a wrong answer here sends a Codex account down
    // Claude's path and silently does nothing.
    let out = run_in(root.path(), &["serve", "oncodex", "--tool", "codex"], &path);
    assert!(out.contains("oncodex"), "{out}");
    let store = root.path().join(".local/share/swapdex");
    assert!(
        store.join("serving-codex").exists(),
        "a Codex account is served through Codex's own pointer"
    );
    assert!(
        !store.join("serving-claude").exists(),
        "and never through Claude's"
    );

    let out = run_in(root.path(), &["serve", "onclaude"], &path);
    assert!(out.contains("onclaude"), "{out}");
    assert!(store.join("serving-claude").exists());
}

/// An install that silently did NOTHING looks exactly like one that worked. Two
/// copies on PATH, or shims still calling the copy you replaced, produce a tool
/// that keeps running the old binary while every update reports success. That
/// went unnoticed here for a full day, and once more when a scope typo made
/// every `npm i -g` a 404 nobody read. doctor answers the question directly.
#[test]
fn doctor_reports_which_swapdex_is_actually_in_use() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());

    // A second, decoy swapdex on PATH - a stale install from another installer.
    let decoy_dir = root.path().join("other-installer");
    std::fs::create_dir_all(&decoy_dir).unwrap();
    std::fs::write(decoy_dir.join("swapdex"), b"#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            decoy_dir.join("swapdex"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let real_dir = std::path::Path::new(bin()).parent().unwrap().to_path_buf();
    let path = format!(
        "{}:{}:{}",
        bin_dir.display(),
        decoy_dir.display(),
        real_dir.display()
    );

    let out = Command::new(bin())
        .args(["doctor"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(
        o.contains("2 copies on PATH"),
        "the shadowed install is named: {o}"
    );
    assert!(
        o.contains("keep one installer"),
        "and the fix comes with it: {o}"
    );
    // The version check reaches a registry, which a test must never do.
    assert!(
        !o.contains("latest is"),
        "no network from a sandboxed run: {o}"
    );
}

/// A sign-in must ask the tool to SIGN IN, and must not be routed through the
/// proxy on the way. The dashboard's key used to run a bare `codex` off PATH -
/// which is our own shim, and the shim puts the proxy in front of any Codex run
/// it does not recognise as plain. A bare launch is not recognised, so the
/// sign-in talked to the proxy, which answered with the account it was already
/// serving: an account with no login of its own came up looking signed in.
///
/// Both entry points now share one runner, and this exercises it through the
/// command line - the dashboard's key reaches the same function, with the
/// account's home added.
#[test]
fn signing_in_asks_the_tool_to_sign_in_and_bypasses_the_proxy() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());

    // A recognisable real `codex`, plus our shim ahead of it on PATH.
    std::fs::write(
        bin_dir.join("codex"),
        b"#!/bin/sh\necho \"REAL-CODEX args=$*\" >> \"$SX_PROBE\"\nexit 0\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            bin_dir.join("codex"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_in(
        root.path(),
        &["run", "work", "--tool", "codex", "--no-launch"],
        &path,
    );
    run_in(root.path(), &["shim"], &path);

    // The shim dir goes FIRST, which is what `swapdex shim` arranges on a real
    // machine. Without it a bare `codex` finds the real one anyway and the test
    // proves nothing - it passed against the very bug it was written for.
    let shim_dir = root.path().join(".local/share/swapdex/bin");
    assert!(
        shim_dir.join("codex").is_file(),
        "the codex shim was written"
    );
    let path = format!("{}:{}", shim_dir.display(), path);

    let probe = root.path().join("probe.log");
    let out = Command::new(bin())
        .args(["login", "work", "--tool", "codex"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .env("SX_PROBE", &probe)
        .output()
        .unwrap();
    let seen = std::fs::read_to_string(&probe).unwrap_or_default();
    let o = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        seen.contains("REAL-CODEX"),
        "codex was reached at all: probe={seen:?} out={o}"
    );
    // The one that matters. A bare launch is not a "plain" run to the shim, so it
    // gets the provider overrides - and then the sign-in talks to the proxy,
    // which answers with the account it is already serving.
    assert!(
        !seen.contains("model_provider=swapdex"),
        "the sign-in must not go through the proxy: probe={seen:?}"
    );
    assert!(
        seen.contains("args=login"),
        "and asks to sign in rather than opening a session: probe={seen:?}"
    );
}
