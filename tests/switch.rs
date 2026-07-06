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
fn ls_empty_store_guides_onboarding() {
    let root = tempfile::tempdir().unwrap();
    let (o, _e, c) = run(root.path(), &["ls"]);
    assert_eq!(c, 0);
    assert!(o.contains("No accounts saved"));
    assert!(
        o.contains("swapdex setup"),
        "empty state should point to setup: {o}"
    );
}

// setup is interactive; piped (non-tty) it degrades instead of hanging.
#[test]
fn setup_non_tty_degrades_gracefully() {
    let root = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(bin())
        .arg("setup")
        .env("SWAPDEX_ROOT", root.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("interactive"),
        "setup should explain it needs a terminal"
    );
}

// Drive the interactive setup wizard over a pipe (SWAPDEX_ASSUME_TTY bypasses
// the tty check). Answers are one per line.
fn run_setup(root: &Path, input: &str) -> (String, i32) {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(bin())
        .arg("setup")
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

// The wizard saves the current login from a typed name.
#[test]
fn setup_wizard_saves_account_from_prompts() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (o, c) = run_setup(root.path(), "mycodex\nn\n");
    assert_eq!(c, 0, "{o}");
    let (ls, _e, _c) = run(root.path(), &["ls"]);
    assert!(
        ls.contains("mycodex"),
        "setup should save the account: {ls}"
    );
}

// The wizard re-prompts on an invalid name instead of skipping it.
#[test]
fn setup_reprompts_on_an_invalid_name() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (o, c) = run_setup(root.path(), "bad/name\ngood\nn\n");
    assert_eq!(c, 0);
    assert!(
        o.contains("can't be a name"),
        "should reject the invalid name: {o}"
    );
    let (ls, _e, _c) = run(root.path(), &["ls"]);
    assert!(ls.contains("good"), "should save the valid retry: {ls}");
}

// login --tool claude: with Claude already logged in it guides the
// `swapdex add` step rather than spawning an interactive session. A fake
// `claude` on PATH makes this deterministic on machines (like CI) that do not
// have the real CLI installed.
#[test]
fn login_claude_guides_the_add_step() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    // Seed a live Claude login so the "already logged in" guidance path runs and
    // it never spawns `claude` interactively (which would hang the test).
    seed_claude(root.path(), "uuid-A", "a@x.com");
    let bin_dir = root.path().join("fakebin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("claude");
    std::fs::write(&fake, "#!/bin/sh\necho 1.0.0\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(bin())
        .args(["login", "work", "--tool", "claude"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &bin_dir)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(o.contains("swapdex add work --tool claude"), "{o}");
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

// Mixed cross-tool live state: ls --json marks each profile active per-tool, not
// a flat bool that stars both.
#[test]
fn ls_json_reports_active_per_tool_in_mixed_state() {
    let root = tempfile::tempdir().unwrap();
    // claude live = uuid-A (profile work); codex live = acct-B (profile home)
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex_tok(root.path(), "acct-A", "AT", "2026-07-04T00:00:00Z");
    run(root.path(), &["add", "work"]);
    seed_codex_tok(root.path(), "acct-B", "AT2", "2026-07-04T00:00:00Z");
    run(root.path(), &["add", "home", "--tool", "codex"]);
    let (o, _e, c) = run(root.path(), &["ls", "--json"]);
    assert_eq!(c, 0);
    let rows: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    let get = |name: &str| -> Vec<String> {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap()["active_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        get("work"),
        vec!["claude-code"],
        "work is active only for claude"
    );
    assert_eq!(get("home"), vec!["codex"], "home is active only for codex");
}

// A stray/legacy file in the store dir must not break a switch (swapdex derives
// the active account from the live login, not any stored hint).
#[test]
fn stray_file_in_store_is_ignored() {
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
    assert_ne!(c, 101, "must not panic on a stray file");
    assert_eq!(c, 0);
}

// The bare `swapdex` prints the ASCII wordmark; piped output carries no ANSI.
#[test]
fn no_args_prints_ascii_banner_plain_when_piped() {
    let root = tempfile::tempdir().unwrap();
    let (o, _e, c) = run(root.path(), &[]);
    assert_eq!(c, 0);
    assert!(o.contains('\u{2588}'), "should print block ASCII art");
    assert!(
        !o.contains('\u{1b}'),
        "no ANSI colour codes when stdout is piped"
    );
}

// completions generate a shell script; status --json is valid JSON.
#[test]
fn completions_and_status_json() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (o, _e, c) = run(root.path(), &["completions", "bash"]);
    assert_eq!(c, 0);
    assert!(
        o.contains("swapdex"),
        "completion script should mention swapdex"
    );
    let (o2, _e, c2) = run(root.path(), &["status", "--json"]);
    assert_eq!(c2, 0);
    let v: serde_json::Value =
        serde_json::from_str(o2.trim()).expect("status --json must be valid JSON");
    assert!(v.is_array(), "status --json is an array of tools");
}

fn seed_codex_tok(root: &Path, account_id: &str, access: &str, last_refresh: &str) {
    let d = root.join(".codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode":"chatgpt","OPENAI_API_KEY":"sk-X",
            "tokens":{"id_token":"h.eyJlbWFpbCI6ImFAeC5jb20ifQ.s","access_token":access,
                      "refresh_token":"RT","account_id":account_id},
            "last_refresh":last_refresh}))
        .unwrap(),
    )
    .unwrap();
}

fn seed_claude(root: &Path, uuid: &str, email: &str) {
    let d = root.join(".claude");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "accessToken":"AT","refreshToken":"RT","expiresAt":9999999999999i64,
            "subscriptionType":"max"}}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join(".claude.json"),
        serde_json::to_vec(&serde_json::json!({
            "oauthAccount":{"accountUuid":uuid,"emailAddress":email,"displayName":"X"}}))
        .unwrap(),
    )
    .unwrap();
}

// --tool must reject a typo instead of silently falling through to both.
#[test]
fn tool_typo_is_rejected_not_fail_open() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, e, c) = run(root.path(), &["use", "x", "--tool", "cluade"]);
    assert_ne!(c, 0, "typo'd --tool must be rejected");
    assert!(e.contains("cluade") || e.contains("possible values"), "{e}");
}

// Two accounts with an EMPTY account_id must not compare 'already active' - the
// switch must actually write, never silently keep the wrong account.
#[test]
fn empty_account_id_still_switches() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_tok(root.path(), "", "AAA", "2026-07-04T00:00:00Z");
    run(root.path(), &["add", "p1", "--tool", "codex"]);
    seed_codex_tok(root.path(), "", "BBB", "2026-07-04T00:00:00Z"); // different live login, same empty id
    let (o, _e, c) = run(root.path(), &["use", "p1", "--tool", "codex"]);
    assert_eq!(c, 0);
    assert!(
        !o.contains("already active"),
        "empty ids must not be 'already active': {o}"
    );
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(
        live["tokens"]["access_token"], "AAA",
        "must actually switch to p1"
    );
}

// A both-tool profile with a stale Codex snapshot must show (stale) in ls, even
// though claude-code sorts first.
#[test]
fn ls_shows_stale_codex_in_a_both_tool_profile() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex_tok(root.path(), "acct-A", "AT", "2020-01-01T00:00:00Z"); // stale codex
    run(root.path(), &["add", "work"]); // both tools
    let (o, _e, c) = run(root.path(), &["ls"]);
    assert_eq!(c, 0);
    assert!(
        o.contains("(stale)"),
        "codex staleness must surface in a both-tool profile: {o}"
    );
}

// add <name> default-both attaches a newly-available tool without --update.
#[test]
fn add_default_both_attaches_missing_tool() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com"); // only claude present
    run(root.path(), &["add", "work"]);
    seed_codex_tok(root.path(), "acct-A", "AT", "2026-07-04T00:00:00Z"); // now codex too
    let (o, _e, c) = run(root.path(), &["add", "work"]); // default both, no --update
    assert_eq!(c, 0, "should attach codex without --update: {o}");
    let (ls, _e, _c) = run(root.path(), &["ls"]);
    assert!(
        ls.contains("codex"),
        "codex should now be in the profile: {ls}"
    );
}

// ls marks a Codex profile whose login has not refreshed in a long time as
// stale (its refresh token may have rotated) - a cross-tool safety cue.
#[test]
fn ls_marks_a_stale_codex_profile() {
    let root = tempfile::tempdir().unwrap();
    let d = root.path().join(".codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode":"chatgpt","OPENAI_API_KEY":"sk-X",
            "tokens":{"id_token":"h.eyJlbWFpbCI6ImFAeC5jb20ifQ.s","access_token":"AT",
                      "refresh_token":"RT","account_id":"acct-A"},
            "last_refresh":"2020-01-01T00:00:00Z"}))
        .unwrap(),
    )
    .unwrap();
    run(root.path(), &["add", "old", "--tool", "codex"]);
    let (o, _e, c) = run(root.path(), &["ls"]);
    assert_eq!(c, 0);
    assert!(
        o.contains("(stale)"),
        "old codex login should be flagged stale: {o}"
    );
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

// Recovery story: `use` backs up the outgoing login; `restore` must bring it
// back, and a second `restore` must toggle back again (restore is reversible
// because it backs up the current login before applying).
#[test]
fn restore_brings_back_the_pre_switch_login() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B"); // live login is now B (un-saved)

    let (_o, e, c) = run(root.path(), &["use", "work", "--tool", "codex"]);
    assert_eq!(c, 0, "use failed: {e}");

    // B was never saved as a profile; restore must still bring it back.
    let (o, e, c) = run(root.path(), &["restore", "--tool", "codex"]);
    assert_eq!(c, 0, "restore failed: {e}");
    assert!(o.contains("restored"), "should say what it did: {o}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-B", "B is live again");

    // Toggle back.
    let (_o, e, c) = run(root.path(), &["restore", "--tool", "codex"]);
    assert_eq!(c, 0, "second restore failed: {e}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-A", "toggled back to A");
}

#[test]
fn restore_without_backups_is_a_clear_error() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, e, c) = run(root.path(), &["restore", "--tool", "codex"]);
    assert_eq!(c, 5, "no backup -> exit 5: {e}");
    assert!(e.contains("no backup"), "message should say why: {e}");
}

#[test]
fn restore_dry_run_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["use", "work", "--tool", "codex"]);

    let (o, _e, c) = run(root.path(), &["restore", "--tool", "codex", "--dry-run"]);
    assert_eq!(c, 0);
    assert!(o.contains("would restore"), "dry-run narrates: {o}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-A", "nothing written");
}

// Partial profile: `use` on a profile that lacks a tool the user IS logged into
// must say that tool was left unchanged (silent partial switches confuse).
#[test]
fn use_notes_a_tool_left_unchanged() {
    let root = tempfile::tempdir().unwrap();
    // Logged into codex AND claude, but the profile only saves codex.
    seed_codex(root.path(), "acct-A");
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
    run(root.path(), &["add", "cx", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    let (o, e, c) = run(root.path(), &["use", "cx"]);
    assert_eq!(c, 0, "use failed: {e}");
    assert!(
        (o.clone() + &e).contains("unchanged"),
        "must note claude-code was left unchanged: {o}{e}"
    );
}

fn claude_live_uuid(root: &Path) -> String {
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(".claude.json")).unwrap()).unwrap();
    v["oauthAccount"]["accountUuid"]
        .as_str()
        .unwrap()
        .to_string()
}

// Bare `restore` must undo the LAST SWITCH only - not rewind every tool to its
// newest backup. A codex-only switch followed by a bare restore must leave
// claude-code untouched even though claude has an (older) backup.
#[test]
fn bare_restore_scopes_to_the_last_switch() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-C1", "c1@x.com");
    seed_codex(root.path(), "acct-X");
    run(root.path(), &["add", "p1"]); // saves both tools

    seed_claude(root.path(), "uuid-C2", "c2@x.com"); // live claude now C2
    seed_codex(root.path(), "acct-Y"); // live codex now Y
    let (_o, e, c) = run(root.path(), &["use", "p1"]); // both switch; backups: claude=C2, codex=Y
    assert_eq!(c, 0, "use p1 failed: {e}");
    std::thread::sleep(std::time::Duration::from_millis(1100)); // separate timeline seconds

    seed_codex(root.path(), "acct-Z"); // fresh codex login appears
    let (_o, e, c) = run(root.path(), &["use", "p1", "--tool", "codex"]); // codex-only switch; backup=Z
    assert_eq!(c, 0, "codex-only use failed: {e}");

    let (o, e, c) = run(root.path(), &["restore"]);
    assert_eq!(c, 0, "bare restore failed: {e}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-Z", "codex undone: {o}");
    assert_eq!(
        claude_live_uuid(root.path()),
        "uuid-C1",
        "claude-code was NOT part of the last switch and must stay: {o}{e}"
    );
}

// `use` must warn when the OUTGOING live login is not saved as any profile -
// only the last 2 backups will remember it.
#[test]
fn use_warns_when_outgoing_login_is_unsaved() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-PRECIOUS"); // logged in, never saved
    let (o, e, c) = run(root.path(), &["use", "work", "--tool", "codex"]);
    assert_eq!(c, 0, "use failed: {e}");
    assert!(
        (o + &e).contains("not saved"),
        "must warn the outgoing login is unsaved"
    );
}

#[test]
fn rename_collision_exits_6() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    run(
        root.path(),
        &["add", "personal", "--tool", "codex", "--update"],
    );
    let (_o, e, c) = run(root.path(), &["rename", "work", "personal"]);
    assert_eq!(
        c, 6,
        "collision is 'already exists' (6), not a hard error: {e}"
    );
}

#[test]
fn login_claude_missing_cli_exits_3() {
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["login", "x", "--tool", "claude"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code().unwrap_or(-1),
        3,
        "missing claude CLI must exit 3 like codex"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("PATH"),
        "guidance goes to stderr"
    );
}

// A corrupt live credential file must not block the one command that can fix
// it: `use <good-profile>` should warn, skip the backup, and apply.
#[test]
fn use_replaces_a_corrupt_live_login() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    std::fs::write(root.path().join(".codex/auth.json"), b"NOT JSON{{{").unwrap();

    let (_o, e, c) = run(root.path(), &["use", "work", "--tool", "codex"]);
    assert_eq!(c, 0, "use must recover from a corrupt live file: {e}");
    assert!(
        e.contains("could not be read"),
        "warns about the skipped backup: {e}"
    );
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(
        live["tokens"]["account_id"], "acct-A",
        "good snapshot applied"
    );
}

// status must report an unreadable login file per tool instead of dying
// mid-output, and status --json must mark it rather than claim logged_in:false.
#[test]
fn status_reports_unreadable_login_file() {
    let root = tempfile::tempdir().unwrap();
    let d = root.path().join(".codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("auth.json"), b"NOT JSON{{{").unwrap();

    let (o, e, c) = run(root.path(), &["status"]);
    assert_eq!(c, 0, "status must not abort: {e}");
    assert!(o.contains("unreadable"), "says the file is unreadable: {o}");

    let (o, _e, c) = run(root.path(), &["status", "--json"]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    let codex = v
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool"] == "codex")
        .unwrap();
    assert_eq!(codex["unreadable"], true, "json marks unreadable: {o}");
}

// doctor: a healthy setup reports ok per section and exits 0.
#[test]
fn doctor_healthy_exits_zero() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let (o, e, c) = run(root.path(), &["doctor"]);
    assert_eq!(c, 0, "healthy doctor must exit 0: {o}{e}");
    assert!(o.contains("ok"), "reports ok sections: {o}");
    assert!(
        !o.contains("problem"),
        "no problems on a healthy setup: {o}"
    );
}

// doctor: a corrupt saved snapshot is found, named, and given a remedy; the
// exit code (9) tells scripts that problems exist.
#[test]
fn doctor_flags_corrupt_snapshot_with_remedy() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    // Corrupt the stored snapshot blob.
    let blob = root
        .path()
        .join(".local/share/swapdex/accounts/work/codex/auth");
    std::fs::write(&blob, b"NOT JSON{{{").unwrap();
    let (o, _e, c) = run(root.path(), &["doctor"]);
    assert_eq!(c, 9, "problems -> exit 9: {o}");
    assert!(o.contains("work"), "names the profile: {o}");
    assert!(o.contains("--update"), "gives the remedy: {o}");
}

// doctor: a corrupt LIVE login file is flagged with the use-a-profile remedy.
#[test]
fn doctor_flags_corrupt_live_login() {
    let root = tempfile::tempdir().unwrap();
    let d = root.path().join(".codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("auth.json"), b"NOT JSON{{{").unwrap();
    let (o, _e, c) = run(root.path(), &["doctor"]);
    assert_eq!(c, 9);
    assert!(o.contains("unreadable"), "{o}");
}

// `manpage` prints a roff man page to stdout (consumed by the Homebrew formula
// at install time; also `swapdex manpage > /usr/local/share/man/man1/swapdex.1`).
#[test]
fn manpage_emits_roff() {
    let root = tempfile::tempdir().unwrap();
    let (o, _e, c) = run(root.path(), &["manpage"]);
    assert_eq!(c, 0);
    assert!(
        o.contains(".TH swapdex 1"),
        "roff man header present: {}",
        &o[..o.len().min(80)]
    );
    assert!(o.contains("restore"), "documents the subcommands");
}

// `use -` toggles to the other profile (the daily-driver shortcut: most users
// have exactly work + personal).
#[test]
fn use_dash_toggles_between_two_profiles() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal", "--tool", "codex"]);
    // live is B (= personal). `use -` must flip to work...
    let (o, e, c) = run(root.path(), &["use", "-"]);
    assert_eq!(c, 0, "use - failed: {o}{e}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-A", "toggled to work");
    // ...and again back to personal.
    let (_o, _e, c) = run(root.path(), &["use", "-"]);
    assert_eq!(c, 0);
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-B", "toggled back");
}

#[test]
fn use_dash_with_ambiguity_refuses_with_candidates() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "one", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "two", "--tool", "codex"]);
    seed_codex(root.path(), "acct-C");
    run(root.path(), &["add", "three", "--tool", "codex"]);
    // 3 profiles, no prior switch on the timeline -> ambiguous target.
    let (_o, e, c) = run(root.path(), &["use", "-"]);
    assert_ne!(c, 0, "ambiguous toggle must refuse");
    assert!(
        e.contains("one") || e.contains("swapdex use"),
        "lists a way out: {e}"
    );
}

// Unique-prefix matching on `use`: `use w` finds "work"; an ambiguous prefix
// refuses and lists the candidates instead of guessing.
#[test]
fn use_unique_prefix_matches() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal", "--tool", "codex"]);
    let (o, e, c) = run(root.path(), &["use", "w"]);
    assert_eq!(c, 0, "unique prefix must resolve: {e}");
    assert!(
        (o + &e).contains("work"),
        "says which profile it resolved to"
    );
}

#[test]
fn use_ambiguous_prefix_refuses() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "prod-a", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "prod-b", "--tool", "codex"]);
    let (_o, e, c) = run(root.path(), &["use", "prod"]);
    assert_eq!(c, 5, "ambiguous prefix -> no such profile class: {e}");
    assert!(
        e.contains("prod-a") && e.contains("prod-b"),
        "lists candidates: {e}"
    );
}

// status --short: one line for shell prompts / statuslines.
#[test]
fn status_short_is_one_compact_line() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let (o, _e, c) = run(root.path(), &["status", "--short"]);
    assert_eq!(c, 0);
    assert_eq!(o.trim().lines().count(), 1, "exactly one line: {o}");
    assert!(o.contains("codex:work"), "tool:profile pairs: {o}");
}

// On a terminal, `rm` asks y/N instead of demanding --yes (scripts still get
// exit 7 when stdin is not a tty - covered by rm_requires_yes elsewhere).
#[test]
fn rm_confirms_interactively_on_tty() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "victim", "--tool", "codex"]);
    let mut child = Command::new(bin())
        .args(["rm", "victim"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"y\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code().unwrap_or(-1), 0);
    let (ls, _e, _c) = run(root.path(), &["ls"]);
    assert!(!ls.contains("victim"), "profile removed after y: {ls}");
}

// `ls --names`: bare profile names, one per line - for scripts and the
// tab-completion snippet in the docs (no jq needed).
#[test]
fn ls_names_prints_bare_names() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal", "--tool", "codex"]);
    let (o, _e, c) = run(root.path(), &["ls", "--names"]);
    assert_eq!(c, 0);
    assert_eq!(o, "personal\nwork\n", "bare sorted names only: {o:?}");
}

// `add` with no name: on a terminal it suggests a name from the live account
// (like setup); non-interactively it errors with guidance instead of a bare
// clap usage error.
#[test]
fn add_without_name_asks_on_tty() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let mut child = Command::new(bin())
        .arg("add")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Accept the suggested default by pressing enter.
    child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code().unwrap_or(-1), 0);
    let (ls, _e, _c) = run(root.path(), &["ls"]);
    assert!(ls.contains("a"), "saved under the suggested name: {ls}");
}

#[test]
fn add_without_name_errors_helpfully_non_tty() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, e, c) = run(root.path(), &["add"]);
    assert_eq!(c, 2, "non-tty add without a name is an argument error");
    assert!(e.contains("swapdex add <name>"), "guides the fix: {e}");
}
