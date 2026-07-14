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
