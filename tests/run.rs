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
    assert!(
        o.to_lowercase().contains("no slots"),
        "empty-state hint: {o}"
    );
}

fn run_in(root: &Path, args: &[&str], path_env: &str) -> String {
    let out = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("PATH", path_env)
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
        used.contains("default account -> work"),
        "use repoints: {used}"
    );
    // Install the shim (finds the fake claude on PATH as the real one).
    let installed = run_in(root.path(), &["shim"], &path);
    assert!(
        installed.contains("installed the claude shim"),
        "{installed}"
    );
    // Run the shim directly; it should exec the fake claude with the slot dir.
    let shim = root.path().join(".local/share/swapdex/bin/claude");
    let out = Command::new(&shim).output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    let slots = root.path().join(".local/share/swapdex/slots");
    assert!(
        o.lines()
            .any(|l| l.starts_with("CFG=") && l.contains(slots.to_str().unwrap())),
        "the shim launched claude in the default account's slot: {o}"
    );
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
        o.contains("slots") && o.contains("account slot"),
        "slots line: {o}"
    );
    assert!(
        o.contains("default") && o.contains("work"),
        "default line: {o}"
    );
    assert!(o.contains("shim"), "shim line: {o}");
}
