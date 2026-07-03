//! End-to-end switch behavior against an isolated SWAPDEX_ROOT. Never touches a
//! real login: every path resolves under the temp root.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn run(root: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn seed_codex(root: &Path, account_id: &str) {
    let d = root.join(".codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode":"chatgpt","OPENAI_API_KEY":"sk-SENTINEL",
            "tokens":{"id_token":"h.eyJlbWFpbCI6ImFAeC5jb20ifQ.s","access_token":"AT",
                      "refresh_token":"RT","account_id":account_id},
            "last_refresh":"2026-07-03T00:00:00Z"}))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn add_use_roundtrip_and_egress_sentinel() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, e, c) = run(root.path(), &["add", "work", "--tool", "codex"]);
    assert_eq!(c, 0, "add failed: {e}");

    seed_codex(root.path(), "acct-B"); // a different live login
    run(root.path(), &["add", "home", "--tool", "codex"]);

    let (_o, e, c) = run(root.path(), &["use", "work", "--tool", "codex"]);
    assert_eq!(c, 0, "use failed: {e}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(
        live["tokens"]["account_id"], "acct-A",
        "live login is now work"
    );

    // EGRESS (A11): no command prints the sentinel token to stdout/stderr.
    for args in [vec!["ls"], vec!["status"], vec!["ls", "--json"]] {
        let (o, e, _c) = run(root.path(), &args);
        assert!(
            !o.contains("SENTINEL") && !e.contains("SENTINEL"),
            "token leak in {args:?}: {o}{e}"
        );
    }
}

#[test]
fn status_trusts_live_identity_not_stale_active_json() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    run(root.path(), &["use", "work", "--tool", "codex"]);
    // The user "logs in directly" to a different account behind swapdex's back.
    seed_codex(root.path(), "acct-Z");
    let (o, _e, _c) = run(root.path(), &["status"]);
    // active.json still says "work", but status must reflect the LIVE account,
    // which is now unsaved.
    assert!(
        o.contains("not saved"),
        "status must reconcile against the live login: {o}"
    );
}

#[test]
fn use_nonexistent_profile_exits_nonzero() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, _e, c) = run(root.path(), &["use", "ghost", "--tool", "codex"]);
    assert_ne!(c, 0);
}

#[test]
fn ls_empty_store_is_friendly_exit_0() {
    let root = tempfile::tempdir().unwrap();
    let (o, _e, c) = run(root.path(), &["ls"]);
    assert_eq!(c, 0);
    assert!(o.contains("no saved profiles"));
}

// C3: a profile name must not escape the store.
#[test]
fn profile_name_traversal_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, _e, c) = run(root.path(), &["add", "../escape", "--tool", "codex"]);
    assert_eq!(c, 2, "traversal name must be rejected");
    assert!(!root.path().join(".local/share/escape").exists());
    let (_o, _e, c2) = run(root.path(), &["rm", "../escape", "--yes"]);
    assert_eq!(c2, 2);
}

// H1/BUG3: switching to the already-active account writes nothing and appends no
// timeline event.
#[test]
fn use_already_active_is_a_noop() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let (o, _e, c) = run(root.path(), &["use", "work", "--tool", "codex"]);
    assert_eq!(c, 0);
    assert!(o.contains("already active"), "{o}");
    let tl = std::fs::read_to_string(root.path().join(".local/share/swapdex/timeline.jsonl"))
        .unwrap_or_default();
    assert!(
        !tl.contains("\"account\":\"work\""),
        "no-op must not append a timeline event: {tl}"
    );
}

// rename moves the profile and ls reflects the new name.
#[test]
fn rename_moves_the_profile() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let (_o, e, c) = run(root.path(), &["rename", "work", "job"]);
    assert_eq!(c, 0, "rename failed: {e}");
    let (o, _e, _c) = run(root.path(), &["ls"]);
    assert!(o.contains("job"), "ls should show renamed profile: {o}");
}

// BUG2: a hand-corrupted non-object active.json must not panic on switch.
#[test]
fn corrupt_active_json_does_not_panic() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    std::fs::write(
        root.path().join(".local/share/swapdex/active.json"),
        b"[1,2,3]",
    )
    .unwrap();
    seed_codex(root.path(), "acct-B"); // force a real switch
    let (_o, _e, c) = run(root.path(), &["use", "work", "--tool", "codex"]);
    assert_ne!(c, 101, "must not panic on a non-object active.json");
    assert_eq!(c, 0);
}

// BUG1: right after a fresh Claude login there is no ~/.claude.json yet; add must
// still work.
#[test]
fn add_works_without_claude_json() {
    let root = tempfile::tempdir().unwrap();
    let d = root.path().join(".claude");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "accessToken":"AT","refreshToken":"RT","expiresAt":9999999999999i64,
            "subscriptionType":"max"}}))
        .unwrap(),
    )
    .unwrap();
    let (_o, e, c) = run(root.path(), &["add", "work", "--tool", "claude"]);
    assert_eq!(c, 0, "add must work without .claude.json: {e}");
}
