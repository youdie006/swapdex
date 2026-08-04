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
    let out = Command::new(bin())
        .args(["doctor"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
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
    // Shim dir NOT on PATH: a plain `claude` resolves to the real (fake) one.
    let out = run_in(root.path(), &["doctor"], &path);
    assert!(
        out.contains("NOT taking effect") && out.contains("PATH"),
        "bypassed shim is called out with the PATH fix: {out}"
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
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
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
