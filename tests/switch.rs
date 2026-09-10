//! End-to-end switch behavior against an isolated SWAPDEX_ROOT. Never touches a
//! real login: every path resolves under the temp root.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn run(root: &Path, args: &[&str]) -> (String, String, i32) {
    run_env(root, args, &[])
}

fn run_env(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(bin());
    cmd.args(args).env("SWAPDEX_ROOT", root);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn chmod600(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
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
    chmod600(&root.join(".codex/auth.json"));
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

// setup must not let you name a profile "-" - it is reserved for `use -`
// (toggle) and `add`/`rename` already reject it. Regression: setup's inline
// save bypassed that guard, creating a "-" profile that broke `use -`.
#[test]
fn setup_rejects_the_reserved_dash_name() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (o, c) = run_setup(root.path(), "-\ngood\nn\n");
    assert_eq!(c, 0);
    assert!(o.contains("reserved"), "must reject '-' as reserved: {o}");
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(
        !names.lines().any(|l| l == "-"),
        "no profile literally named '-': {names}"
    );
    assert!(names.contains("good"), "the valid retry is saved: {names}");
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
    // Exit 3: guidance only, nothing saved - `login x && use x` in a
    // script must not proceed.
    assert_eq!(out.status.code().unwrap_or(-1), 3, "{o}");
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
    chmod600(&root.join(".claude/.credentials.json"));
    chmod600(&root.join(".claude.json"));
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
    // The note names the tool it is about: one stale login among several is
    // not the account being stale.
    assert!(
        o.contains("codex stale"),
        "codex staleness must surface, and say it is codex: {o}"
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
        o.contains("codex stale"),
        "old codex login should be flagged, and named: {o}"
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
    // An empty PATH, deliberately: doctor now counts how many swapdex copies are
    // reachable, and a developer machine with a cargo install AND target/debug is
    // genuinely two - a real finding that has nothing to do with this setup being
    // healthy, and one that would make the answer depend on whose machine it is.
    let (o, e, c) = run_env(root.path(), &["doctor"], &[("PATH", "")]);
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

// A just-lapsed Claude ACCESS token (they expire ~hourly and refresh silently)
// must NOT make `status` cry "access token expired" - that is the false-alarm
// the user hit daily. Only a >30-day-old login gets a soft re-login note.
#[test]
fn status_does_not_flag_a_normally_lapsed_access_token() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".claude")).unwrap();
    // expiresAt one hour in the PAST - the everyday auto-refresh case.
    let one_hour_ago_ms = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        - 3_600_000;
    std::fs::write(
        root.path().join(".claude/.credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"AT","refreshToken":"RT","expiresAt":{one_hour_ago_ms},"subscriptionType":"max"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        root.path().join(".claude.json"),
        br#"{"oauthAccount":{"accountUuid":"u","emailAddress":"a@x.com","displayName":"D"}}"#,
    )
    .unwrap();
    let (o, _e, c) = run(root.path(), &["status"]);
    assert_eq!(c, 0);
    assert!(o.contains("a@x.com"), "shows the account: {o}");
    assert!(
        !o.to_lowercase().contains("expired"),
        "a normally-lapsed access token is not 'expired': {o}"
    );
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
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert_eq!(names, "a\n", "saved under the suggested name: {names:?}");
}

#[test]
fn add_without_name_errors_helpfully_non_tty() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, e, c) = run(root.path(), &["add"]);
    assert_eq!(c, 2, "non-tty add without a name is an argument error");
    assert!(e.contains("swapdex add <name>"), "guides the fix: {e}");
}

// REVIEW-1 (must-fix): `use ""` (e.g. an unset shell variable) must NOT match
// the single profile as a "unique prefix" and switch - it is an invalid name.
#[test]
fn use_empty_string_never_switches() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-UNSAVED"); // live: an unsaved account
    let (_o, _e, c) = run(root.path(), &["use", ""]);
    assert_eq!(c, 2, "empty name is invalid, never a prefix match");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(
        live["tokens"]["account_id"], "acct-UNSAVED",
        "live login untouched"
    );
}

// REVIEW-6: `rm ghost` must say "no profile" without first asking y/N.
#[test]
fn rm_nonexistent_does_not_prompt() {
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["rm", "ghost"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(Stdio::null()) // would EOF the prompt if one appeared
        .output()
        .unwrap();
    assert_eq!(
        out.status.code().unwrap_or(-1),
        5,
        "straight to 'no profile'"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("delete saved profile"),
        "no confirmation prompt for a ghost"
    );
}

// REVIEW-5: with the live identity unreadable/empty, `use -` must not
// "toggle" right back to the profile of the newest switch (= where you are).
#[test]
fn use_dash_never_repicks_the_newest_switch_destination() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "a", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "b", "--tool", "codex"]);
    seed_codex(root.path(), "acct-C");
    run(root.path(), &["add", "c", "--tool", "codex"]);
    run(root.path(), &["use", "a", "--tool", "codex"]);
    // Live identity degrades (empty account_id): nothing matches a profile.
    seed_codex(root.path(), "");
    let (_o, e, _c) = run(root.path(), &["use", "-"]);
    assert!(
        !e.contains("'-' -> 'a'"),
        "must not re-pick the newest switch destination: {e}"
    );
}

// ls aligns by DISPLAY width: a CJK profile name (2 columns per char) must not
// shear the table for the profiles after it.
#[test]
fn ls_aligns_cjk_names_by_display_width() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "회사계정", "--tool", "codex"]); // 4 chars, 8 columns
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal", "--tool", "codex"]); // 8 chars, 8 columns
    let (o, _e, c) = run(root.path(), &["ls"]);
    assert_eq!(c, 0);
    // Both rows must place the email at the same DISPLAY column (CJK ~ 2).
    fn disp_prefix(l: &str) -> usize {
        let idx = l.find("a@x.com").unwrap();
        l[..idx]
            .chars()
            .map(|c| if (c as u32) >= 0x1100 { 2 } else { 1 })
            .sum()
    }
    let widths: Vec<usize> = o
        .lines()
        .filter(|l| l.contains("a@x.com"))
        .map(disp_prefix)
        .collect();
    assert_eq!(widths.len(), 2, "both rows visible: {o}");
    assert_eq!(
        widths[0], widths[1],
        "email column must align in display columns: {o}"
    );
}

// `ui`: a numbered interactive picker. Piped "2" selects the second profile
// and performs the switch; plain Enter cancels without touching anything.
fn run_ui(root: &Path, input: &str) -> (String, String, i32) {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(bin())
        .arg("ui")
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
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn ui_picker_switches_by_number() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);
    // live is B (= beta). The list is sorted: 1) alpha 2) beta. Pick 1.
    let (o, e, c) = run_ui(root.path(), "1\n");
    assert_eq!(c, 0, "picker switch failed: {o}{e}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-A", "switched to alpha");
}

#[test]
fn ui_picker_enter_cancels() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);
    let (o, _e, c) = run_ui(root.path(), "\n");
    assert_eq!(c, 0);
    assert!(
        o.contains("cancel") || o.contains("nothing"),
        "says it did nothing: {o}"
    );
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(live["tokens"]["account_id"], "acct-B", "nothing switched");
}

#[test]
fn ui_picker_rejects_bad_number_then_accepts() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);
    let (o, e, c) = run_ui(root.path(), "9\n1\n");
    assert_eq!(c, 0, "{o}{e}");
    let live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(
        live["tokens"]["account_id"], "acct-A",
        "re-prompt then switch"
    );
}

#[test]
fn ui_non_tty_degrades_gracefully() {
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_ne!(out.status.code().unwrap_or(-1), 0);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("terminal"),
        "explains it needs a terminal"
    );
}

// After a switch, picking a numbered session hands off to the official flow:
// swapdex execs `sessionwiki resume <id>` (one-shot handoff on explicit human
// action - the same precedent as `login` spawning `codex login`).
#[test]
fn ui_resume_pick_execs_sessionwiki() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);

    // Session fixture + a fake `sessionwiki` that proves the exec happened.
    let fixture = root.path().join("sessions.json");
    std::fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!([
            {"id":"aaa111","tool":"codex","title":"fix the retry loop",
             "started":"2099-01-01T00:00:00Z"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let bin_dir = root.path().join("fakebin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("sessionwiki");
    std::fs::write(&fake, "#!/bin/sh\necho \"RESUMED $4\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("SWAPDEX_SESSIONWIKI_JSON", &fixture)
        .env("PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Pick profile 1 (alpha), then resume session 1.
    child.stdin.as_mut().unwrap().write_all(b"1\n1\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(
        o.contains("RESUMED aaa111"),
        "exec'd sessionwiki resume: {o}"
    );
}

// Enter at the resume prompt skips - no exec, clean exit.
#[test]
fn ui_resume_enter_skips() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);
    let fixture = root.path().join("sessions.json");
    std::fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!([
            {"id":"aaa111","tool":"codex","title":"t","started":"2099-01-01T00:00:00Z"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("SWAPDEX_SESSIONWIKI_JSON", &fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"1\n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(!o.contains("RESUMED"), "no exec on skip: {o}");
}

// sessionwiki installed but with an EMPTY index (never `sessionwiki sync`ed)
// must not hide the real on-disk sessions: the menu falls back to the native
// reader instead of showing a blank list.
#[test]
fn ui_empty_sessionwiki_falls_back_to_native_sessions() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    // sessionwiki present (fixture) but its index is EMPTY.
    let fixture = root.path().join("sessions.json");
    std::fs::write(&fixture, b"[]").unwrap();
    // A real native codex transcript on disk.
    let cx = root.path().join(".codex/sessions/2026/07/14");
    std::fs::create_dir_all(&cx).unwrap();
    std::fs::write(
        cx.join("rollout-2026-07-14T09-00-00-0a000000-0000-4000-8000-0000000000dd.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":"x"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message",
                "message":"wire up the websocket reconnect"}}),
        ),
    )
    .unwrap();
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("SWAPDEX_SESSIONWIKI_JSON", &fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"1\n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(
        o.contains("wire up the websocket reconnect"),
        "the on-disk session shows even with an empty sessionwiki index: {o}"
    );
}

// REVIEW: a multibyte session id must not panic the ui hint (byte-6 slice).
#[test]
fn ui_hint_survives_multibyte_session_id() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);
    let fixture = root.path().join("sessions.json");
    std::fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!([
            {"id":"a日本語id","tool":"codex","title":"t","started":"2099-01-01T00:00:00Z"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("SWAPDEX_SESSIONWIKI_JSON", &fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"1\n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "no panic on multibyte id: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// REVIEW: the any-account fallback must fire on the FIRST real switch (the
// timeline check has to happen before use_account writes the switch event).
#[test]
fn ui_hint_fallback_fires_on_first_real_switch() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);
    // A session that PREDATES every switch (nothing attributable).
    let fixture = root.path().join("sessions.json");
    std::fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!([
            {"id":"aaa111","tool":"codex","title":"old work","started":"2000-01-01T00:00:00Z"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("SWAPDEX_SESSIONWIKI_JSON", &fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Pick alpha = a REAL switch (live is beta).
    child.stdin.as_mut().unwrap().write_all(b"1\n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(
        o.contains("recent sessions"),
        "fallback hint must appear on the first real switch: {o}"
    );
}

fn seed_gemini(root: &Path, sub: &str, email: &str) {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let d = root.join(".gemini");
    std::fs::create_dir_all(&d).unwrap();
    let payload = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&serde_json::json!({"sub": sub, "email": email})).unwrap());
    std::fs::write(
        d.join("oauth_creds.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token":"AT-SENTINEL","refresh_token":"RT-SENTINEL",
            "id_token": format!("h.{payload}.s"),
            "expiry_date": 9999999999999i64,
            "scope":"openid","token_type":"Bearer"}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        d.join("google_accounts.json"),
        serde_json::to_vec(&serde_json::json!({"active": email, "old": []})).unwrap(),
    )
    .unwrap();
    chmod600(&root.join(".gemini/oauth_creds.json"));
    chmod600(&root.join(".gemini/google_accounts.json"));
}

// Gemini: add/use roundtrip switches BOTH files together, ls shows the email,
// and no command leaks the sentinel tokens.
#[test]
fn gemini_add_use_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    seed_gemini(root.path(), "sub-A", "a@example.com");
    let (_o, e, c) = run(root.path(), &["add", "gwork", "--tool", "gemini"]);
    assert_eq!(c, 0, "add failed: {e}");
    seed_gemini(root.path(), "sub-B", "b@example.com");
    run(root.path(), &["add", "ghome", "--tool", "gemini"]);

    let (o, e, c) = run(root.path(), &["use", "gwork", "--tool", "gemini"]);
    assert_eq!(c, 0, "use failed: {o}{e}");
    let oauth: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join(".gemini/oauth_creds.json")).unwrap(),
    )
    .unwrap();
    assert!(
        oauth["id_token"].as_str().unwrap().contains("."),
        "oauth swapped"
    );
    let accounts: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join(".gemini/google_accounts.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        accounts["active"], "a@example.com",
        "accounts swapped together"
    );

    let (ls, _e, _c) = run(root.path(), &["ls"]);
    assert!(ls.contains("a@example.com"), "identity shown: {ls}");
    assert!(ls.contains("[gemini") || ls.contains("gemini"), "{ls}");
    for args in [vec!["ls"], vec!["status"], vec!["ls", "--json"]] {
        let (o, e, _c) = run(root.path(), &args);
        assert!(
            !o.contains("SENTINEL") && !e.contains("SENTINEL"),
            "token leak in {args:?}"
        );
    }
}

// A three-tool machine: one profile holds claude + codex + gemini and a single
// `use` switches all three.
#[test]
fn three_tool_profile_switches_together() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex(root.path(), "acct-A");
    seed_gemini(root.path(), "sub-A", "a@example.com");
    run(root.path(), &["add", "all-a"]);
    seed_claude(root.path(), "uuid-B", "b@x.com");
    seed_codex(root.path(), "acct-B");
    seed_gemini(root.path(), "sub-B", "b@example.com");
    run(root.path(), &["add", "all-b"]);

    let (o, e, c) = run(root.path(), &["use", "all-a"]);
    assert_eq!(c, 0, "{o}{e}");
    assert_eq!(claude_live_uuid(root.path()), "uuid-A");
    let codex: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(codex["tokens"]["account_id"], "acct-A");
    let g: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join(".gemini/google_accounts.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(g["active"], "a@example.com");
    assert_eq!(o.matches("switched").count(), 3, "all three switched: {o}");
}

fn seed_antigravity(root: &Path, refresh: &str) {
    let d = root.join(".gemini").join("antigravity-cli");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("antigravity-oauth-token"),
        serde_json::to_vec(&serde_json::json!({
            "token": {"access_token":"AT-SENTINEL","token_type":"Bearer",
                       "refresh_token": refresh,
                       "expiry":"2026-07-06T10:09:19.638+09:00"},
            "auth_method":"consumer"}))
        .unwrap(),
    )
    .unwrap();
    chmod600(&root.join(".gemini/antigravity-cli/antigravity-oauth-token"));
}

// Antigravity: single-file swap roundtrip; identity has no email (none is
// stored on disk) but a stable token fingerprint matches profiles.
#[test]
fn antigravity_add_use_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    seed_antigravity(root.path(), "RT-SENTINEL-A");
    let (_o, e, c) = run(root.path(), &["add", "aw", "--tool", "antigravity"]);
    assert_eq!(c, 0, "add failed: {e}");
    seed_antigravity(root.path(), "RT-SENTINEL-B");
    run(root.path(), &["add", "ah", "--tool", "antigravity"]);

    let (o, e, c) = run(root.path(), &["use", "aw", "--tool", "antigravity"]);
    assert_eq!(c, 0, "use failed: {o}{e}");
    let tok: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            root.path()
                .join(".gemini/antigravity-cli/antigravity-oauth-token"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(tok["token"]["refresh_token"], "RT-SENTINEL-A", "swapped");

    // Already-active no-op works via the fingerprint (no email/id on disk).
    let (o, _e, c) = run(root.path(), &["use", "aw", "--tool", "antigravity"]);
    assert_eq!(c, 0);
    assert!(o.contains("already active"), "{o}");

    // Egress: the refresh token never appears in any output.
    for args in [
        vec!["ls"],
        vec!["status"],
        vec!["ls", "--json"],
        vec!["status", "--json"],
    ] {
        let (o, e, _c) = run(root.path(), &args);
        assert!(
            !o.contains("SENTINEL") && !e.contains("SENTINEL"),
            "token leak in {args:?}: {o}{e}"
        );
    }
}

// THE flow real use demanded: already logged into account A, want to ADD
// account B. `login <name> --tool claude` must save A, sign out locally, run
// claude for the fresh sign-in, and capture B - one command.
fn fake_claude(root: &Path, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = root.join("fakebin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("claude");
    // swapdex now drives claude as `claude auth logout` (sign-out) and
    // `claude auth login` (sign-in). Inject a guard so every fake no-ops on
    // the logout and only its body runs for login / bare.
    let wrapped = script.replacen(
        "#!/bin/sh\n",
        "#!/bin/sh\ncase \"$1 $2\" in \"auth logout\") exit 0 ;; esac\n",
        1,
    );
    std::fs::write(&fake, wrapped).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin_dir
}

fn run_login_tty(root: &Path, bin_dir: &Path, args: &[&str], input: &str) -> (String, String, i32) {
    use std::io::Write;
    use std::process::Stdio;
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("PATH", &path)
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
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn login_claude_adds_a_second_account_in_one_flow() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "old", "--tool", "claude"]);
    // Fake claude: simulates the user signing into account B inside the app.
    // Must answer the `claude --version` probe without touching credentials.
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.claude"
cat > "$SWAPDEX_ROOT/.claude/.credentials.json" <<'CRED'
{"claudeAiOauth":{"accessToken":"AT-B","refreshToken":"RT-B","expiresAt":9999999999999,"subscriptionType":"pro"}}
CRED
cat > "$SWAPDEX_ROOT/.claude.json" <<'CFG'
{"oauthAccount":{"accountUuid":"uuid-B","emailAddress":"b@x.com","displayName":"B"},"projects":{"/keep/me":{"trust":true}}}
CFG
"#;
    let bin_dir = fake_claude(root.path(), script);
    // "y" confirms the sign-out-and-sign-in flow.
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "newacc", "--tool", "claude"],
        "y\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    assert_eq!(claude_live_uuid(root.path()), "uuid-B", "B is live: {o}");
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(names.contains("newacc"), "B saved as newacc: {names}");
    assert!(names.contains("old"), "A's profile still there: {names}");
    // And the old account is one command away.
    let (_o, e, c) = run(root.path(), &["use", "old"]);
    assert_eq!(c, 0, "{e}");
    assert_eq!(
        claude_live_uuid(root.path()),
        "uuid-A",
        "A restored via use"
    );
}

#[test]
fn login_claude_restores_original_when_no_new_signin() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "old", "--tool", "claude"]);
    // Fake claude that exits WITHOUT logging in (user quit / login failed).
    let bin_dir = fake_claude(root.path(), "#!/bin/sh\nexit 0\n");
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "newacc", "--tool", "claude"],
        "y\n",
    );
    assert_eq!(c, 8, "incomplete login flow: {o}{e}");
    assert_eq!(
        claude_live_uuid(root.path()),
        "uuid-A",
        "original login restored - NEVER lost: {o}{e}"
    );
}

// Switch -> conversation opens immediately: 'c' at the post-switch menu execs
// claude; `use --open --tool codex` execs codex right after the switch.
#[test]
fn ui_post_switch_c_opens_claude() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    // The profile must HAVE claude for 'c' to open it (a codex-only profile
    // correctly refuses 'c' - see plain_menu_offers_only_the_profiles_tools).
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha"]);
    seed_claude(root.path(), "uuid-B", "b@x.com");
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta"]);
    let bin_dir = fake_claude(root.path(), "#!/bin/sh\necho CLAUDE-OPENED\n");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"1\nc\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(
        o.contains("CLAUDE-OPENED"),
        "claude exec'd after switch: {o}"
    );
}

#[test]
fn use_open_execs_the_tool() {
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    // fake codex binary
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = root.path().join("fakebin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("codex");
    std::fs::write(&fake, "#!/bin/sh\necho CODEX-OPENED\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin())
        .args(["use", "alpha", "--tool", "codex", "--open"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(o.contains("switched codex"), "{o}");
    assert!(o.contains("CODEX-OPENED"), "codex exec'd after switch: {o}");
}

// Folder choice: the launched conversation opens IN the chosen directory
// (Claude/Codex sessions are per-directory).
#[test]
fn use_open_dir_launches_in_that_folder() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    let bin_dir = root.path().join("fakebin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("codex");
    std::fs::write(&fake, "#!/bin/sh\necho \"OPENED-IN $(pwd)\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let proj = root.path().join("myproject");
    std::fs::create_dir_all(&proj).unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin())
        .args([
            "use",
            "alpha",
            "--tool",
            "codex",
            "--open",
            "--dir",
            proj.to_str().unwrap(),
        ])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    // canonicalize: on macOS the tempdir sits behind the /var -> /private/var
    // symlink, and the child's pwd reports the RESOLVED path.
    let proj = proj.canonicalize().unwrap();
    assert!(
        o.contains(&format!("OPENED-IN {}", proj.display())),
        "launched in the chosen folder: {o}"
    );
}

// No sessionwiki anywhere: the post-switch menu still lists recent sessions
// (read natively from ~/.claude / ~/.codex) and resumes via the tool's own
// mechanism, in the session's own folder.
#[test]
fn post_switch_native_sessions_without_sessionwiki() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alpha", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "beta", "--tool", "codex"]);
    // A native claude session on disk, with a real cwd recorded.
    let proj_dir = root.path().join("myproj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    let store = root.path().join(".claude/projects/-myproj");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("0a000000-0000-4000-8000-0000000000aa.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({"type":"user","cwd":proj_dir.to_str().unwrap(),
                "message":{"content":[{"type":"text","text":"fix the flaky retry test"}]}}),
        ),
    )
    .unwrap();
    // Fake claude proves the native resume exec: prints its args and pwd.
    let bin_dir = fake_claude(
        root.path(),
        "#!/bin/sh\necho \"RESUME-ARGS $@\"\necho \"RESUME-PWD $(pwd)\"\n",
    );
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("PATH", &path) // no sessionwiki in this PATH
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"1\n1\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(
        o.contains("fix the flaky retry"),
        "native session listed: {o}"
    );
    assert!(
        o.contains("RESUME-ARGS --resume 0a000000-0000-4000-8000-0000000000aa"),
        "claude --resume exec'd: {o}"
    );
    // canonicalized for the macOS /var -> /private/var symlink (see above).
    let proj_dir = proj_dir.canonicalize().unwrap();
    assert!(
        o.contains(&format!("RESUME-PWD {}", proj_dir.display())),
        "opened in the session's own folder: {o}"
    );
}

// Real-use angle-testing round: setup's "add another" must ask WHICH tool
// (the old block was Codex-only - the root of "it keeps asking about Codex"),
// and the login tool question must re-prompt on garbage instead of bailing.
#[test]
fn setup_add_another_asks_which_tool() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let mut child = Command::new(bin())
        .arg("setup")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // name the found login, say yes to "add another", then cancel at the
    // tool question, then no to the loop.
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"work\ny\n\nn\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(
        o.contains("which tool?") && o.contains("4) Antigravity"),
        "add-another asks the tool, all four listed: {o}"
    );
    assert!(
        !o.contains("add another Codex account"),
        "the Codex-only prompt is gone: {o}"
    );
}

#[test]
fn login_tool_question_reprompts_on_garbage() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(bin())
        .args(["login", "newone"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"7\n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{o}");
    assert!(
        o.contains("pick a number between 1 and 4"),
        "garbage re-prompts: {o}"
    );
    assert!(o.contains("cancelled"), "Enter cancels: {o}");
}

// THE deep account bug: refresh tokens ROTATE while you use an account. If
// switching away only writes the outgoing login to the backup ring, the
// matched profile keeps day-one tokens - and switching back later restores a
// refresh token the provider may have already revoked. Switching away must
// refresh the matched profile's snapshot with the live capture.
#[test]
fn switch_away_refreshes_matched_profile_snapshot() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal", "--tool", "codex"]);
    run(root.path(), &["use", "work"]);
    // Simulate the provider rotating acct-A's tokens during a long session.
    let auth = root.path().join(".codex/auth.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
    v["tokens"]["access_token"] = "AT-ROTATED".into();
    v["tokens"]["refresh_token"] = "RT-ROTATED".into();
    std::fs::write(&auth, serde_json::to_string(&v).unwrap()).unwrap();
    // Switch away, then back.
    let (_o, e, c) = run(root.path(), &["use", "personal"]);
    assert_eq!(c, 0, "{e}");
    let (_o, e, c) = run(root.path(), &["use", "work"]);
    assert_eq!(c, 0, "{e}");
    let live = std::fs::read_to_string(&auth).unwrap();
    assert!(
        live.contains("RT-ROTATED"),
        "switching back restores the ROTATED tokens, not day-one ones: {live}"
    );
}

// Same invariant for the login flow's stash: if the current login matches a
// saved profile, the stash (its freshest tokens) must refresh that profile.
#[test]
fn login_flow_refreshes_matched_profile_from_stash() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "old", "--tool", "claude"]);
    // Rotate the live tokens after the profile was saved.
    let cred = root.path().join(".claude/.credentials.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cred).unwrap()).unwrap();
    v["claudeAiOauth"]["refreshToken"] = "RT-ROTATED".into();
    std::fs::write(&cred, serde_json::to_string(&v).unwrap()).unwrap();
    // Add account B through the login flow. The fake must answer --version
    // WITHOUT touching credentials - swapdex probes `claude --version` first.
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.claude"
cat > "$SWAPDEX_ROOT/.claude/.credentials.json" <<'CRED'
{"claudeAiOauth":{"accessToken":"AT-B","refreshToken":"RT-B","expiresAt":9999999999999,"subscriptionType":"pro"}}
CRED
cat > "$SWAPDEX_ROOT/.claude.json" <<'CFG'
{"oauthAccount":{"accountUuid":"uuid-B","emailAddress":"b@x.com","displayName":"B"},"projects":{}}
CFG
"#;
    let bin_dir = fake_claude(root.path(), script);
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "newacc", "--tool", "claude"],
        "y\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    // Back to the old account: must carry the rotated token.
    let (_o, e, c) = run(root.path(), &["use", "old"]);
    assert_eq!(c, 0, "{e}");
    let live = std::fs::read_to_string(&cred).unwrap();
    assert!(
        live.contains("RT-ROTATED"),
        "profile 'old' was refreshed from the stash: {live}"
    );
}

// Deep account-security angle: snapshots ARE tokens. If anything loosens a
// mode inside the store (backup tools, cp -r, umask accidents), opening the
// store must tighten it back - 0700 dirs, 0600 files, all the way down.
#[test]
fn store_open_tightens_loose_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let dir = root.path().join(".local/share/swapdex/accounts/work/codex");
    let file = dir.join("auth");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    // Any store-opening command repairs the modes.
    run(root.path(), &["ls"]);
    let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    let fmode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(dmode, 0o700, "dir tightened");
    assert_eq!(fmode, 0o600, "token file tightened");
}

// Deep-dig round 2: the rotation invariant must hold on EVERY path that
// touches the live login, not just `use` switching away.
#[test]
fn restore_refreshes_outgoing_matched_profile() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal", "--tool", "codex"]);
    // Live is personal (B) after its add; switch to work so a backup of B
    // exists and A is live.
    run(root.path(), &["use", "work"]);
    // Rotation while on work.
    let auth = root.path().join(".codex/auth.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
    v["tokens"]["refresh_token"] = "RT-ROTATED".into();
    std::fs::write(&auth, serde_json::to_string(&v).unwrap()).unwrap();
    // restore = undo the switch (back to personal); outgoing work must be
    // refreshed with the rotated tokens.
    let (_o, e, c) = run(root.path(), &["restore"]);
    assert_eq!(c, 0, "{e}");
    let (_o, e, c) = run(root.path(), &["use", "work"]);
    assert_eq!(c, 0, "{e}");
    let live = std::fs::read_to_string(&auth).unwrap();
    assert!(
        live.contains("RT-ROTATED"),
        "restore refreshed 'work': {live}"
    );
}

#[test]
fn use_noop_still_refreshes_profile() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let auth = root.path().join(".codex/auth.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
    v["tokens"]["refresh_token"] = "RT-ROTATED".into();
    std::fs::write(&auth, serde_json::to_string(&v).unwrap()).unwrap();
    let (o, _e, c) = run(root.path(), &["use", "work"]);
    assert_eq!(c, 0);
    assert!(o.contains("already active"), "{o}");
    // The no-op must still sync the profile snapshot with the live tokens.
    let stored = std::fs::read_to_string(
        root.path()
            .join(".local/share/swapdex/accounts/work/codex/auth"),
    )
    .unwrap();
    assert!(
        stored.contains("RT-ROTATED"),
        "no-op use refreshed the snapshot: {stored}"
    );
}

// `add --update` while logged into a DIFFERENT account must not silently
// repoint the profile - that changes what the name means.
#[test]
fn add_update_refuses_account_change_noninteractive() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    let (_o, e, c) = run(root.path(), &["add", "work", "--tool", "codex", "--update"]);
    assert_eq!(c, 7, "refused: {e}");
    assert!(
        e.contains("different account"),
        "says WHY and what it holds: {e}"
    );
    // Profile still holds acct-A.
    let stored = std::fs::read_to_string(
        root.path()
            .join(".local/share/swapdex/accounts/work/codex/auth"),
    )
    .unwrap();
    assert!(stored.contains("acct-A"), "unchanged: {stored}");
}

// THE core journey must work for ALL FOUR tools, not just claude: already
// logged into A, `login <name> --tool T` adds account B in one flow (save
// current, sign out locally, run the tool's own sign-in, capture B).
fn fake_tool(root: &Path, bin: &str, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = root.join("fakebin").join(bin);
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join(bin);
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin_dir
}

#[test]
fn login_gemini_adds_a_second_account_in_one_flow() {
    let root = tempfile::tempdir().unwrap();
    seed_gemini(root.path(), "a@x.com", "sub-A");
    run(root.path(), &["add", "old", "--tool", "gemini"]);
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.gemini"
printf '%s' '{"access_token":"AT-B","refresh_token":"RT-B","id_token":"h.eyJlbWFpbCI6ImJAeC5jb20iLCJzdWIiOiJzdWItQiJ9.s","expiry_date":9999999999999}' > "$SWAPDEX_ROOT/.gemini/oauth_creds.json"
printf '%s' '{"active":"b@x.com"}' > "$SWAPDEX_ROOT/.gemini/google_accounts.json"
"#;
    let bin_dir = fake_tool(root.path(), "gemini", script);
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "gemini"],
        "y\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(names.contains("second"), "B saved: {names}");
    // Old account one command away, with its original identity.
    let (_o, e, c) = run(root.path(), &["use", "old"]);
    assert_eq!(c, 0, "{e}");
    let creds = std::fs::read_to_string(root.path().join(".gemini/oauth_creds.json")).unwrap();
    assert!(creds.contains("RT-SENTINEL"), "A restored: {creds}");
}

#[test]
fn login_antigravity_adds_a_second_account_in_one_flow() {
    let root = tempfile::tempdir().unwrap();
    seed_antigravity(root.path(), "RT-A");
    run(root.path(), &["add", "old", "--tool", "antigravity"]);
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.gemini/antigravity-cli"
printf '%s' '{"token":{"access_token":"AT-B","refresh_token":"RT-B","expiry":"2027-01-01T00:00:00Z","token_type":"Bearer"},"auth_method":"consumer"}' > "$SWAPDEX_ROOT/.gemini/antigravity-cli/antigravity-oauth-token"
"#;
    let bin_dir = fake_tool(root.path(), "agy", script);
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "antigravity"],
        "y\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(names.contains("second") && names.contains("old"), "{names}");
    let (_o, e, c) = run(root.path(), &["use", "old"]);
    assert_eq!(c, 0, "{e}");
    let tok = std::fs::read_to_string(
        root.path()
            .join(".gemini/antigravity-cli/antigravity-oauth-token"),
    )
    .unwrap();
    assert!(tok.contains("RT-A"), "A restored: {tok}");
}

#[test]
fn login_gemini_restores_original_when_no_new_signin() {
    let root = tempfile::tempdir().unwrap();
    seed_gemini(root.path(), "a@x.com", "sub-A");
    run(root.path(), &["add", "old", "--tool", "gemini"]);
    let bin_dir = fake_tool(
        root.path(),
        "gemini",
        "#!/bin/sh\ncase \"$1\" in --version) echo 1.0.0; exit 0;; esac\nexit 0\n",
    );
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "gemini"],
        "y\n",
    );
    assert_eq!(c, 8, "incomplete: {o}{e}");
    let creds = std::fs::read_to_string(root.path().join(".gemini/oauth_creds.json")).unwrap();
    // The original seed's tokens, byte-identical (ids live inside JWT b64).
    assert!(creds.contains("RT-SENTINEL"), "A NEVER lost: {creds}");
}

// Ctrl+C during the interactive sign-in goes to the whole foreground process
// group. swapdex must SURVIVE it and run the restore-stash branch - dying
// would leave the user locally signed out of everything.
#[test]
fn login_survives_sigint_and_restores() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "old", "--tool", "claude"]);
    // Fake claude: SIGINTs its parent (swapdex) mid-login, then exits
    // without completing any sign-in.
    let script = "#!/bin/sh\ncase \"$1\" in --version) echo 1.0.0; exit 0;; esac\nkill -INT $PPID\nsleep 0.3\nexit 130\n";
    let bin_dir = fake_claude(root.path(), script);
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "claude"],
        "y\n",
    );
    assert_eq!(c, 8, "survived SIGINT, reported incomplete: {o}{e}");
    assert!(e.contains("was restored"), "restore branch ran: {e}");
    assert_eq!(
        claude_live_uuid(root.path()),
        "uuid-A",
        "previous login restored, NOT left signed out"
    );
}

// Walkthrough-audit HIGHs.

// Enter-through setup must attach every logged-in tool to the suggested
// profile, not scare the user with "replace it?" and silently skip 3 of 4.
#[test]
fn setup_enter_through_attaches_all_tools() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    // Same email on both tools -> the SAME suggested name, which is where
    // the old "replace it?" prompt silently skipped the second tool.
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex(root.path(), "acct-A");
    let mut child = Command::new(bin())
        .arg("setup")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Enter for each tool's default name, then 'n' to add-another.
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"\n\n\nn\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(!o.contains("replace it?"), "no replace scare: {o}");
    let (l, _e, _c) = run(root.path(), &["ls"]);
    assert!(
        l.contains("claude-code") && l.contains("codex"),
        "both tools attached to one profile: {l}"
    );
}

// A corrupt LIVE ~/.claude.json must be diagnosed as such - not blamed on
// the profile snapshot with a remedy that also fails - and doctor must flag it.
#[test]
fn corrupt_live_claude_config_diagnosed_not_blamed_on_snapshot() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "alice", "--tool", "claude"]);
    seed_claude(root.path(), "uuid-B", "b@x.com");
    run(root.path(), &["add", "bob", "--tool", "claude"]);
    std::fs::write(root.path().join(".claude.json"), b"not json{{").unwrap();
    // Correct perms, so doctor's permission check cannot mask the real test:
    // diagnosing the CORRUPTION itself.
    std::fs::set_permissions(
        root.path().join(".claude.json"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let (_o, e, c) = run(root.path(), &["use", "alice", "--tool", "claude"]);
    assert_ne!(c, 0);
    assert!(
        e.contains(".claude.json") && e.to_lowercase().contains("live"),
        "blames the LIVE file, not the snapshot: {e}"
    );
    let (o, _e, c) = run(root.path(), &["doctor"]);
    assert_eq!(c, 9, "doctor flags it: {o}");
    assert!(o.contains(".claude.json"), "doctor names the file: {o}");
}

// `add` with a valid live login but a corrupt ~/.claude.json (a hand-edited
// JSON syntax error - very common) must NOT report "not logged in" (exit 3):
// the user IS logged in, the config is broken. It is a hard error (exit 1)
// with the corrupt-file remedy, so they fix the file instead of re-logging in.
#[test]
fn add_with_corrupt_config_is_not_reported_as_not_logged_in() {
    let root = tempfile::tempdir().unwrap();
    // Valid credential file, corrupt config.
    std::fs::create_dir_all(root.path().join(".claude")).unwrap();
    std::fs::write(
        root.path().join(".claude/.credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"AT","refreshToken":"RT","expiresAt":9999999999999,"subscriptionType":"max"}}"#,
    )
    .unwrap();
    std::fs::write(root.path().join(".claude.json"), b"BROKEN { not json").unwrap();
    let (_o, e, c) = run(root.path(), &["add", "p", "--tool", "claude"]);
    assert_eq!(
        c, 1,
        "corrupt config is a hard error, not not-logged-in: {e}"
    );
    assert!(
        !e.contains("not logged in"),
        "must not claim the user is logged out when they are: {e}"
    );
    assert!(
        e.contains(".claude.json"),
        "points at the corrupt file: {e}"
    );
}

// A present-but-uncapturable live login (a valid Gemini oauth_creds.json with a
// corrupt google_accounts.json) must NOT be overwritten without a backup: the
// switch refuses for that tool rather than destroying a recoverable login.
#[test]
fn use_refuses_to_overwrite_a_present_uncapturable_login() {
    let root = tempfile::tempdir().unwrap();
    seed_gemini(root.path(), "sub-a", "a@g.com");
    run(root.path(), &["add", "ga", "--tool", "gemini"]);
    seed_gemini(root.path(), "sub-b", "b@g.com");
    // b is now live and UNSAVED. Corrupt only the auxiliary file so present()
    // still sees the login but capture() fails.
    std::fs::write(
        root.path().join(".gemini/google_accounts.json"),
        b"NOT JSON {",
    )
    .unwrap();
    let before = std::fs::read(root.path().join(".gemini/oauth_creds.json")).unwrap();
    let (_o, e, c) = run(root.path(), &["use", "ga", "--tool", "gemini"]);
    assert_ne!(c, 0, "the switch must not report success: {e}");
    assert!(
        e.contains("refusing to overwrite"),
        "explains the refusal: {e}"
    );
    let after = std::fs::read(root.path().join(".gemini/oauth_creds.json")).unwrap();
    assert_eq!(before, after, "the live login B is untouched (not lost)");
}

// A per-tool failure must not abort the whole multi-tool switch: the other
// tools still switch, and a summary says what failed.
#[test]
fn use_continues_past_a_failing_tool() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alice"]);
    seed_claude(root.path(), "uuid-B", "b@x.com");
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "bob"]);
    // Corrupt alice's CODEX snapshot in the store.
    std::fs::write(
        root.path()
            .join(".local/share/swapdex/accounts/alice/codex/auth"),
        b"broken{{",
    )
    .unwrap();
    let (o, e, c) = run(root.path(), &["use", "alice"]);
    assert_eq!(c, 1, "partial switch exits 1: {o}{e}");
    assert_eq!(claude_live_uuid(root.path()), "uuid-A", "claude DID switch");
    assert!(
        e.contains("codex") && e.contains("failed"),
        "summary names the failed tool: {e}"
    );
    assert!(e.contains("restore"), "points at the undo: {e}");
}

// Same for add: a corrupt live login for one tool must not silently create a
// partial profile with exit 1 and no explanation.
#[test]
fn add_reports_a_failing_tool_and_keeps_going() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex(root.path(), "acct-A");
    std::fs::write(root.path().join(".codex/auth.json"), b"broken{{").unwrap();
    let (o, e, c) = run(root.path(), &["add", "prof"]);
    assert_eq!(c, 1, "{o}{e}");
    assert!(
        o.contains("saved profile 'prof'"),
        "claude still saved: {o}"
    );
    assert!(
        e.contains("codex") && (e.contains("skipped") || e.contains("could not")),
        "explains the skipped tool: {e}"
    );
}

// Walkthrough-audit MEDIUMs.
#[test]
fn login_reserved_name_dash_rejected() {
    let root = tempfile::tempdir().unwrap();
    let (_o, e, c) = run(root.path(), &["login", "-", "--tool", "codex"]);
    assert_ne!(c, 0, "'-' is the toggle, never a profile name: {e}");
}

#[test]
fn use_typo_is_one_line() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work"]);
    let (o, e, c) = run(root.path(), &["use", "zzz"]);
    assert_eq!(c, 5);
    assert!(
        !o.contains("left unchanged") && !e.contains("left unchanged"),
        "no per-tool noise for a typo: {o}{e}"
    );
}

#[test]
fn multi_tool_tier_prefers_claude_over_antigravity() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_antigravity(root.path(), "RT-A");
    run(root.path(), &["add", "all2"]);
    let (o, _e, _c) = run(root.path(), &["ls"]);
    assert!(
        o.contains("[max]"),
        "claude's real plan tier, not antigravity's auth_method: {o}"
    );
}

#[test]
fn rename_rewrites_timeline_attribution() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "alice", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "bob", "--tool", "codex"]);
    run(root.path(), &["use", "alice"]);
    run(root.path(), &["rename", "alice", "corp"]);
    let tl =
        std::fs::read_to_string(root.path().join(".local/share/swapdex/timeline.jsonl")).unwrap();
    assert!(
        !tl.contains("\"alice\"") && tl.contains("\"corp\""),
        "timeline follows the rename: {tl}"
    );
}

// Walkthrough-audit LOWs.
#[test]
fn ls_hides_ghost_dirs_and_unknown_tools() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "bee", "--tool", "codex"]);
    std::fs::create_dir_all(root.path().join(".local/share/swapdex/accounts/ghosty")).unwrap();
    std::fs::create_dir_all(
        root.path()
            .join(".local/share/swapdex/accounts/bee/fakedir"),
    )
    .unwrap();
    let (o, _e, _c) = run(root.path(), &["ls"]);
    assert!(!o.contains("ghosty"), "empty dir is not a profile: {o}");
    assert!(!o.contains("fakedir"), "unknown subdir is not a tool: {o}");
}

#[test]
fn whitespace_only_name_rejected() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    let (_o, e, c) = run(root.path(), &["add", "   ", "--tool", "codex"]);
    assert_eq!(c, 2, "{e}");
}

#[test]
fn doctor_flags_loose_live_cred_perms() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    std::fs::set_permissions(
        root.path().join(".codex/auth.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let (o, _e, c) = run(root.path(), &["doctor"]);
    assert_eq!(c, 9, "{o}");
    assert!(
        o.contains("auth.json") && o.contains("chmod 600"),
        "names the loose live file with the remedy: {o}"
    );
}

// Delta-hunt findings on the bug-sweep itself.

// F1: an UNREADABLE target snapshot must not bypass the repoint guard -
// corrupt-or-absent must not be conflated when the profile dir exists.
#[test]
fn login_repoint_guard_holds_for_unreadable_snapshot() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-C");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    std::fs::write(
        root.path()
            .join(".local/share/swapdex/accounts/work/codex/auth"),
        b"corrupt{{",
    )
    .unwrap();
    seed_codex(root.path(), "acct-A");
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.codex"
printf '%s' '{"auth_mode":"chatgpt","tokens":{"id_token":"h.eyJlbWFpbCI6ImJAeC5jb20ifQ.s","access_token":"AT-B","refresh_token":"RT-B","account_id":"acct-B"},"last_refresh":"2026-07-08T00:00:00Z"}' > "$SWAPDEX_ROOT/.codex/auth.json"
"#;
    let bin_dir = fake_tool(root.path(), "codex", script);
    // y = flow, Enter = skip keep-current, n = refuse repoint, Enter = skip rescue.
    let (o, e, _c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "work", "--tool", "codex"],
        "y\n\nn\n\n",
    );
    assert!(
        o.contains("Repoint"),
        "guard must fire even when the stored snapshot is unreadable: {o}{e}"
    );
    let stored = std::fs::read_to_string(
        root.path()
            .join(".local/share/swapdex/accounts/work/codex/auth"),
    )
    .unwrap();
    assert!(
        !stored.contains("acct-B"),
        "refusing must keep 'work' un-repointed: {stored}"
    );
}

// F2: refusing the repoint must OFFER to save the completed sign-in under a
// different name - a real browser login must not be silently discarded.
#[test]
fn login_repoint_refusal_offers_rescue_name() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-C");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-A");
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.codex"
printf '%s' '{"auth_mode":"chatgpt","tokens":{"id_token":"h.eyJlbWFpbCI6ImJAeC5jb20ifQ.s","access_token":"AT-B","refresh_token":"RT-B","account_id":"acct-B"},"last_refresh":"2026-07-08T00:00:00Z"}' > "$SWAPDEX_ROOT/.codex/auth.json"
"#;
    let bin_dir = fake_tool(root.path(), "codex", script);
    // y = flow, Enter = skip keeping the (unmatched) current account,
    // n = refuse repoint, "rescued" = save B under that name.
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "work", "--tool", "codex"],
        "y\n\nn\nrescued\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(
        names.contains("rescued"),
        "B saved under the rescue name: {names}"
    );
    assert!(names.contains("work"), "work untouched: {names}");
}

// F3/F4: ghost dirs (no known tools) are not profiles - rename must not act
// on them as source, and colliding with one as target is a clean exit 6.
#[test]
fn rename_treats_ghost_dirs_consistently() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    std::fs::create_dir_all(
        root.path()
            .join(".local/share/swapdex/accounts/legacy/some-old-tool"),
    )
    .unwrap();
    let (_o, e, c) = run(root.path(), &["rename", "legacy", "revived"]);
    assert_eq!(c, 5, "hidden source is not a profile: {e}");
    std::fs::create_dir_all(root.path().join(".local/share/swapdex/accounts/ghost/junk")).unwrap();
    let (_o, e, c) = run(root.path(), &["rename", "work", "ghost"]);
    assert_eq!(c, 6, "hidden target collision is a clean 'exists' (6): {e}");
}

// F5: SIGQUIT (Ctrl+backslash) must be ridden out like SIGINT.
#[test]
fn login_survives_sigquit_and_restores() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "old", "--tool", "claude"]);
    let script = "#!/bin/sh\ncase \"$1\" in --version) echo 1.0.0; exit 0;; esac\nkill -QUIT $PPID\nsleep 0.3\nexit 131\n";
    let bin_dir = fake_claude(root.path(), script);
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "claude"],
        "y\n",
    );
    assert_eq!(c, 8, "survived SIGQUIT: {o}{e}");
    assert_eq!(claude_live_uuid(root.path()), "uuid-A", "restored");
}

// Upgrade/environment audit findings.

// A future-stamped backup (clock skew during one switch) must not shadow the
// real newest backup forever - restore must undo the LAST switch.
#[test]
fn future_stamped_backup_does_not_hijack_restore() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal", "--tool", "codex"]);
    run(root.path(), &["use", "work"]); // backup: B
                                        // Ghost: a backup stamped in 2030 holding account C.
    let ghost = root
        .path()
        .join(".local/share/swapdex/backups/codex/1900000000000000000");
    std::fs::create_dir_all(&ghost).unwrap();
    std::fs::write(
        ghost.join("auth"),
        serde_json::to_vec(&serde_json::json!({"auth_mode":"chatgpt",
            "tokens":{"id_token":"h.eyJlbWFpbCI6ImNAei5jb20ifQ.s","access_token":"AT-C",
                      "refresh_token":"RT-C","account_id":"acct-C"},
            "last_refresh":"2026-07-01T00:00:00Z"}))
        .unwrap(),
    )
    .unwrap();
    let (o, e, c) = run(root.path(), &["restore", "--tool", "codex"]);
    assert_eq!(c, 0, "{e}");
    let live = std::fs::read_to_string(root.path().join(".codex/auth.json")).unwrap();
    assert!(
        live.contains("acct-B"),
        "restore undoes the LAST switch (B), never the future ghost (C): {o}{live}"
    );
}

// An unwritable store must say WHY, not claim another swapdex is mid-switch.
#[test]
fn unwritable_store_reports_the_real_problem() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let lock = root.path().join(".local/share/swapdex/.lock");
    std::fs::write(&lock, b"").ok();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (_o, e, c) = run(root.path(), &["use", "work"]);
    assert_ne!(c, 0);
    assert!(
        !e.contains("mid-switch"),
        "EACCES is not a lock contention: {e}"
    );
    assert!(
        e.to_lowercase().contains("perm") || e.to_lowercase().contains("writable"),
        "says the real cause: {e}"
    );
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
}

// A legacy all-whitespace profile (0.2.x allowed them) must stay MANAGEABLE:
// rm/rename/use may act on it even though creation now rejects it.
#[test]
fn legacy_whitespace_profile_is_manageable() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    // Simulate the 0.2.x-created profile by renaming the store dir directly.
    std::fs::rename(
        root.path().join(".local/share/swapdex/accounts/work"),
        root.path().join(".local/share/swapdex/accounts/ "),
    )
    .unwrap();
    let (_o, e, c) = run(root.path(), &["rename", " ", "fixed"]);
    assert_eq!(c, 0, "legacy name must be renamable: {e}");
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(names.contains("fixed"), "{names}");
}

// Two separate single-tool switches inside the same wall-clock second must
// NOT be treated as one invocation - a bare restore undoes only the LAST one.
#[test]
fn restore_scopes_to_last_invocation_not_same_second() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work"]);
    seed_claude(root.path(), "uuid-B", "b@x.com");
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "personal"]);
    run(root.path(), &["use", "work"]);
    // Two back-to-back single-tool switches (same second, near-certainly).
    run(root.path(), &["use", "personal", "--tool", "codex"]);
    run(root.path(), &["use", "personal", "--tool", "claude"]);
    let (o, e, c) = run(root.path(), &["restore"]);
    assert_eq!(c, 0, "{e}");
    assert!(
        o.contains("claude") && !o.contains("restored codex"),
        "only the LAST invocation (claude) is undone: {o}"
    );
    // codex stays on personal (B).
    let auth = std::fs::read_to_string(root.path().join(".codex/auth.json")).unwrap();
    assert!(auth.contains("acct-B"), "codex untouched: {auth}");
}

// Bare `swapdex` over a pipe (no tty) prints the banner, never the TUI - and
// never creates the store on a fresh machine.
#[test]
fn bare_swapdex_pipe_is_banner_not_tui() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    let (o, _e, c) = run(root.path(), &[]);
    assert_eq!(c, 0);
    assert!(o.contains("swapdex"), "banner printed: {o}");
    assert!(o.contains("active:"), "shows where you stand: {o}");
}

#[test]
fn bare_swapdex_fresh_machine_is_banner_and_does_not_create_store() {
    let root = tempfile::tempdir().unwrap();
    let (o, _e, c) = run(root.path(), &[]);
    assert_eq!(c, 0);
    assert!(o.contains("swapdex setup"), "fresh-machine hint: {o}");
    assert!(
        !root.path().join(".local/share/swapdex").exists(),
        "a bare banner must not create the store"
    );
}

// Security F1: a symlinked accounts/<name> must not let a token write escape
// the 0700 store (the leaf-only symlink check missed intermediate dirs).
#[test]
fn add_refuses_symlinked_profile_dir() {
    let root = tempfile::tempdir().unwrap();
    let escape = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    // Pre-create the store, then plant accounts/beta -> attacker dir.
    run(root.path(), &["add", "seed", "--tool", "codex"]);
    let accounts = root.path().join(".local/share/swapdex/accounts");
    std::os::unix::fs::symlink(escape.path(), accounts.join("beta")).unwrap();
    let (_o, e, c) = run(root.path(), &["add", "beta", "--tool", "codex"]);
    assert_ne!(c, 0, "must refuse: {e}");
    // Nothing was written into the attacker's dir.
    assert!(
        !escape.path().join("codex/auth").exists(),
        "no token escaped the store into {}",
        escape.path().display()
    );
}

// Real-use bug: adding a NEW account but the tool signs you back into the
// SAME one (cached browser session). swapdex must NOT save a duplicate under
// the new name, must restore the login, and must explain how to switch.
#[test]
fn login_same_account_back_does_not_save_duplicate() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    // Fake codex whose "login" writes the SAME account back.
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; logout) rm -f "$SWAPDEX_ROOT/.codex/auth.json"; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.codex"
printf '%s' '{"auth_mode":"chatgpt","tokens":{"id_token":"h.eyJlbWFpbCI6ImFAeC5jb20ifQ.s","access_token":"AT2","refresh_token":"RT2","account_id":"acct-A"},"last_refresh":"2026-07-08T00:00:00Z"}' > "$SWAPDEX_ROOT/.codex/auth.json"
"#;
    let bin_dir = fake_tool(root.path(), "codex", script);
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "codex"],
        "y\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    assert!(
        e.contains("SAME account"),
        "explains the same-account outcome: {e}"
    );
    assert!(
        e.to_lowercase().contains("browser"),
        "tells how to actually switch: {e}"
    );
    // 'second' must NOT exist - no duplicate profile of acct-A.
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(
        !names.contains("second"),
        "no duplicate profile saved: {names}"
    );
    // The live login is intact (still acct-A).
    let auth = std::fs::read_to_string(root.path().join(".codex/auth.json")).unwrap();
    assert!(auth.contains("acct-A"), "login restored/intact: {auth}");
}

// The login flow must NOT hold the store lock across the interactive sign-in
// - otherwise a left-open login blocks rename/use/everything with "another
// swapdex is mid-switch" (the real macOS report). The fake tool, WHILE it is
// the spawned sign-in, runs a `rename` against the same store; it must
// succeed, proving the lock was released.
#[test]
fn login_does_not_hold_lock_during_signin() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    // A second profile we will rename FROM INSIDE the spawned sign-in.
    seed_codex(root.path(), "acct-V");
    run(root.path(), &["add", "victim", "--tool", "codex"]);
    // Put the current account back so the login flow stashes acct-A.
    seed_codex(root.path(), "acct-A");
    // Fake codex: on `login`, rename victim -> moved (using the real swapdex
    // binary) BEFORE writing the new account. If login still held the lock,
    // this rename would fail with exit 4.
    let script = format!(
        r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; logout) rm -f "$SWAPDEX_ROOT/.codex/auth.json"; exit 0;; esac
"{}" rename victim moved > "$SWAPDEX_ROOT/rename-rc.txt" 2>&1
echo "rc=$?" >> "$SWAPDEX_ROOT/rename-rc.txt"
mkdir -p "$SWAPDEX_ROOT/.codex"
printf '%s' '{{"auth_mode":"chatgpt","tokens":{{"id_token":"h.eyJlbWFpbCI6ImJAeC5jb20ifQ.s","access_token":"ATB","refresh_token":"RTB","account_id":"acct-B"}},"last_refresh":"2026-07-08T00:00:00Z"}}' > "$SWAPDEX_ROOT/.codex/auth.json"
"#,
        bin()
    );
    let bin_dir = fake_tool(root.path(), "codex", &script);
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "codex"],
        "\ny\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    let rc = std::fs::read_to_string(root.path().join("rename-rc.txt")).unwrap_or_default();
    assert!(
        rc.contains("rc=0"),
        "rename during the sign-in succeeded (lock was free): {rc}"
    );
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(
        names.contains("moved") && !names.contains("victim"),
        "victim was renamed mid-sign-in: {names}"
    );
}

// Codex login uses the device-code flow (--device-auth) by DEFAULT so it works
// over SSH / headless (opt out with SWAPDEX_CODEX_LOGIN=browser). The fake
// records its argv to prove swapdex passes the flag.
#[test]
fn codex_login_uses_device_auth_by_default() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-A"); // live = A, matched to 'work'
    let script = r#"#!/bin/sh
case "$1" in --version) echo 1.0.0; exit 0;; logout) rm -f "$SWAPDEX_ROOT/.codex/auth.json"; exit 0;; esac
printf '%s\n' "$@" > "$SWAPDEX_ROOT/codex-argv.txt"
mkdir -p "$SWAPDEX_ROOT/.codex"
printf '%s' '{"auth_mode":"chatgpt","tokens":{"id_token":"h.eyJlbWFpbCI6ImJAeC5jb20ifQ.s","access_token":"AT-B","refresh_token":"RT-B","account_id":"acct-B"},"last_refresh":"2026-07-08T00:00:00Z"}' > "$SWAPDEX_ROOT/.codex/auth.json"
"#;
    let bin_dir = fake_tool(root.path(), "codex", script);
    let (o, e, _c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "codex"],
        "\ny\n",
    );
    let argv = std::fs::read_to_string(root.path().join("codex-argv.txt"))
        .unwrap_or_else(|_| panic!("fake codex login was never spawned: {o}{e}"));
    assert!(
        argv.contains("login") && argv.contains("--device-auth"),
        "codex login must get --device-auth by default: {argv:?}"
    );
}

// A codex switch is refused while a `codex` process is running - the running-
// session guard (Claude's 0.25.0 analog for Codex). This is the "use a codex MCP
// from Claude, the account logs out" class: swapping the shared auth.json under a
// running codex lets its next token refresh revoke the account. --force overrides.
#[test]
fn codex_switch_is_guarded_while_a_codex_runs() {
    let acct = |r: &Path| -> String {
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(r.join(".codex/auth.json")).unwrap()).unwrap();
        v["tokens"]["account_id"].as_str().unwrap().to_string()
    };
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "work", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B"); // the live login is B now

    // With a codex running, `use work` (acct-A) is refused; live stays B.
    let (o, e, c) = run_env(
        root.path(),
        &["use", "work", "--tool", "codex"],
        &[("SWAPDEX_TEST_RUNNING", "codex")],
    );
    assert_ne!(c, 0, "switch must be refused while a codex runs: {o}{e}");
    assert!(
        format!("{o}{e}").contains("running codex session"),
        "the guard message is shown: {o}{e}"
    );
    assert_eq!(
        acct(root.path()),
        "acct-B",
        "the live login was NOT switched"
    );

    // --force overrides -> switches to work (acct-A).
    let (o2, e2, c2) = run_env(
        root.path(),
        &["use", "work", "--tool", "codex", "--force"],
        &[("SWAPDEX_TEST_RUNNING", "codex")],
    );
    assert_eq!(c2, 0, "--force switches anyway: {o2}{e2}");
    assert_eq!(acct(root.path()), "acct-A", "the forced switch applied");
}

// Same rotation-logout guard for Gemini (also non-slotted, tokens rotate).
#[test]
fn gemini_switch_is_guarded_while_a_gemini_runs() {
    let active = |r: &Path| -> String {
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(r.join(".gemini/google_accounts.json")).unwrap())
                .unwrap();
        v["active"].as_str().unwrap().to_string()
    };
    let root = tempfile::tempdir().unwrap();
    seed_gemini(root.path(), "sub-a", "a@x.com");
    run(root.path(), &["add", "work", "--tool", "gemini"]);
    seed_gemini(root.path(), "sub-b", "b@x.com"); // live is b@x.com

    let (o, e, c) = run_env(
        root.path(),
        &["use", "work", "--tool", "gemini"],
        &[("SWAPDEX_TEST_RUNNING", "gemini")],
    );
    assert_ne!(c, 0, "refused while a gemini runs: {o}{e}");
    assert!(
        format!("{o}{e}").contains("running gemini session"),
        "guard msg: {o}{e}"
    );
    assert_eq!(
        active(root.path()),
        "b@x.com",
        "the live login was NOT switched"
    );

    let (_o, _e, c2) = run_env(
        root.path(),
        &["use", "work", "--tool", "gemini", "--force"],
        &[("SWAPDEX_TEST_RUNNING", "gemini")],
    );
    assert_eq!(c2, 0, "--force switches anyway");
    assert_eq!(active(root.path()), "a@x.com", "the forced switch applied");
}

// Same rotation-logout guard for Antigravity (non-slotted, refresh token rotates).
#[test]
fn antigravity_switch_is_guarded_while_an_antigravity_runs() {
    let rt = |r: &Path| -> String {
        let v: serde_json::Value = serde_json::from_slice(
            &std::fs::read(r.join(".gemini/antigravity-cli/antigravity-oauth-token")).unwrap(),
        )
        .unwrap();
        v["token"]["refresh_token"].as_str().unwrap().to_string()
    };
    let root = tempfile::tempdir().unwrap();
    seed_antigravity(root.path(), "RT-A");
    run(root.path(), &["add", "work", "--tool", "antigravity"]);
    seed_antigravity(root.path(), "RT-B"); // live is RT-B

    let (o, e, c) = run_env(
        root.path(),
        &["use", "work", "--tool", "antigravity"],
        &[("SWAPDEX_TEST_RUNNING", "antigravity")],
    );
    assert_ne!(c, 0, "refused while an antigravity runs: {o}{e}");
    assert!(
        format!("{o}{e}").contains("running antigravity session"),
        "guard msg: {o}{e}"
    );
    assert_eq!(rt(root.path()), "RT-B", "the live login was NOT switched");

    let (_o, _e, c2) = run_env(
        root.path(),
        &["use", "work", "--tool", "antigravity", "--force"],
        &[("SWAPDEX_TEST_RUNNING", "antigravity")],
    );
    assert_eq!(c2, 0, "--force switches anyway");
    assert_eq!(rt(root.path()), "RT-A", "the forced switch applied");
}

// A safe switcher must NEVER run `claude auth logout` during add-account: that
// revokes the OAuth token server-side, which would kill the snapshot swapdex
// just captured AND every saved profile sharing the account (the "all my
// logins got signed out" disaster). Sign-out is LOCAL only; the fresh login
// prompt for the new account still appears, and the old snapshot stays valid.
#[test]
fn login_claude_signs_out_locally_without_revoking() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "old", "--tool", "claude"]);
    // Fake claude records ANY `auth logout` call (there must be none); a
    // login writes B.
    let script = r#"#!/bin/sh
case "$1 $2" in "auth logout") echo logout >> "$SWAPDEX_ROOT/authcalls.txt"; exit 0 ;; esac
case "$1" in --version) echo 1.0.0; exit 0;; esac
mkdir -p "$SWAPDEX_ROOT/.claude"
printf '%s' '{"claudeAiOauth":{"accessToken":"AT-B","refreshToken":"RT-B","expiresAt":9999999999999,"subscriptionType":"pro"}}' > "$SWAPDEX_ROOT/.claude/.credentials.json"
printf '%s' '{"oauthAccount":{"accountUuid":"uuid-B","emailAddress":"b@x.com","displayName":"B"},"projects":{}}' > "$SWAPDEX_ROOT/.claude.json"
"#;
    // fake_claude injects its own logout guard; use a raw file so ours runs.
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = root.path().join("rawbin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("claude"), script).unwrap();
    std::fs::set_permissions(
        bin_dir.join("claude"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (o, e, c) = run_login_tty(
        root.path(),
        &bin_dir,
        &["login", "second", "--tool", "claude"],
        "y\n",
    );
    assert_eq!(c, 0, "{o}{e}");
    // swapdex must NEVER have invoked the server-revoking logout.
    let calls = std::fs::read_to_string(root.path().join("authcalls.txt")).unwrap_or_default();
    assert!(
        !calls.contains("logout"),
        "swapdex must not run `claude auth logout` - it revokes server-side: {calls:?}"
    );
    // The new account is saved AND the old profile's snapshot is preserved.
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(names.contains("second"), "B saved: {names}");
    assert!(names.contains("old"), "old profile preserved: {names}");
    let old_cred = std::fs::read_to_string(
        root.path()
            .join(".local/share/swapdex/accounts/old/claude-code/credentials"),
    )
    .unwrap_or_default();
    assert!(
        old_cred.contains("\"accessToken\":\"AT\""),
        "old account's saved token is untouched (not revoked): {old_cred}"
    );
}

/// A fake curl for `swapdex quota` (via the SWAPDEX_CURL fixture hook): reads
/// the config from stdin like the real one, answers by bearer token.
fn write_fake_curl(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-curl");
    std::fs::write(
        &p,
        r#"#!/bin/sh
cfg=$(cat)
tok=$(printf '%s' "$cfg" | sed -n 's/.*Authorization: Bearer \([^"]*\)".*/\1/p')
case "$tok" in
  AT) printf '{"five_hour":{"utilization":25.0,"resets_at":1893456000},"seven_day":{"utilization":50.0}}\n200' ;;
  *)  printf '{"type":"error","error":{"type":"authentication_error"}}\n401' ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Seed a saved claude profile directly in the store (bypasses `add`, so the
/// snapshot can hold a DIFFERENT account than the live login).
fn seed_claude_profile(root: &Path, name: &str, uuid: &str, email: &str, token: &str) {
    let d = root
        .join(".local/share/swapdex/accounts")
        .join(name)
        .join("claude-code");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("credentials"),
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"R"}}}}"#),
    )
    .unwrap();
    std::fs::write(
        d.join("oauth_account"),
        format!(r#"{{"accountUuid":"{uuid}","emailAddress":"{email}"}}"#),
    )
    .unwrap();
    chmod600(&d.join("credentials"));
    chmod600(&d.join("oauth_account"));
}

// End-to-end `quota --json` against a fake curl: the active account resolves
// live data, an expired snapshot reports "expired", and the JSON `name` is
// the PLAIN profile name (no " (active)" suffix - scripts feed it to `use`).
#[test]
fn quota_json_reports_accounts_with_clean_names() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "main", "--tool", "claude"]);
    seed_claude_profile(root.path(), "backup", "uuid-B", "b@x.com", "AT-B");
    let curl = write_fake_curl(root.path());
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    assert!(v["offline"].is_null(), "not offline: {v}");
    let accounts = v["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2, "{v}");
    // Active first (stable sort), with a clean name and live windows.
    assert_eq!(accounts[0]["name"], "main", "no ' (active)' suffix: {v}");
    assert_eq!(accounts[0]["active"], true);
    assert_eq!(accounts[0]["status"], "ok");
    assert_eq!(accounts[0]["five_hour"]["remaining_pct"], 75.0);
    // The stale snapshot got a 401 -> expired, never a fake number.
    assert_eq!(accounts[1]["name"], "backup");
    assert_eq!(accounts[1]["status"], "expired");
}

/// Register a slot pointing at `dir`, signed in with `token`. A slot holds its
/// own credential - that is the whole point of the model - and the tool that
/// owns it refreshes that credential in place.
fn seed_slot(root: &Path, name: &str, dir: &Path, token: &str, email: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"R"}}}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        format!(r#"{{"oauthAccount":{{"accountUuid":"u-{name}","emailAddress":"{email}"}}}}"#),
    )
    .unwrap();
    let store = root.join(".local/share/swapdex");
    std::fs::create_dir_all(&store).unwrap();
    let file = store.join("slots.json");
    let mut list: Vec<serde_json::Value> = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    list.push(serde_json::json!({
        "name": name,
        "id": format!("id-{name}"),
        "config_dir": dir,
        "adopted": true,
    }));
    std::fs::write(&file, serde_json::to_vec(&list).unwrap()).unwrap();
}

// A saved profile and a slot can carry the SAME name - the profile is the old
// copied snapshot, the slot is where that account actually lives now. The
// snapshot's token dies when the refresh token rotates; the slot's is refreshed
// in place. Reading the dead one and skipping the live one reported a perfectly
// healthy account as expired.
#[test]
fn a_slot_outranks_a_saved_snapshot_of_the_same_name() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "main", "--tool", "claude"]);
    // 'backup' exists twice: a rotted snapshot, and a slot holding the live "AT".
    seed_claude_profile(root.path(), "backup", "uuid-B", "b@x.com", "DEAD-TOKEN");
    seed_slot(
        root.path(),
        "backup",
        &root.path().join("slot-backup"),
        "AT",
        "b@x.com",
    );
    let curl = write_fake_curl(root.path());
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    let accounts = v["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2, "one row per account, not two: {v}");
    let backup = accounts
        .iter()
        .find(|a| a["name"] == "backup")
        .unwrap_or_else(|| panic!("no backup row: {v}"));
    assert_eq!(
        backup["status"], "ok",
        "the slot's live token answered, not the snapshot's dead one: {v}"
    );
    assert_eq!(backup["five_hour"]["remaining_pct"], 75.0, "{v}");
}

// A corrupt/unusable saved token is a PER-ACCOUNT finding; it must not
// masquerade as "the network is down" and abort the other accounts.
#[test]
fn quota_unusable_token_does_not_abort_the_run() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "main", "--tool", "claude"]);
    seed_claude_profile(root.path(), "backup", "uuid-B", "b@x.com", "AT-B");
    // Corrupt the LIVE token (the active row fetches it first): a quote char
    // can't be embedded in the curl config, so it is locally unusable.
    std::fs::write(
        root.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"bad\"tok","refreshToken":"R"}}"#,
    )
    .unwrap();
    let curl = write_fake_curl(root.path());
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    assert!(
        v["offline"].is_null(),
        "a local token fault must not report the network down: {v}"
    );
    let accounts = v["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2, "both accounts reported: {v}");
    assert_eq!(accounts[0]["status"], "offline");
    assert!(
        accounts[0]["detail"].as_str().unwrap().contains("unusable"),
        "{v}"
    );
    // The healthy snapshot was still fetched (401 -> expired).
    assert_eq!(accounts[1]["status"], "expired");
}

// The plain (dumb-terminal) post-switch menu must offer "new X" only for the
// tools the switched profile holds. A codex-only profile must not offer - nor
// launch - claude (which was NOT switched, so it'd open an unrelated account).
#[test]
fn plain_menu_offers_only_the_profiles_tools() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-A");
    run(root.path(), &["add", "onlycodex", "--tool", "codex"]);
    seed_codex(root.path(), "acct-B");
    run(root.path(), &["add", "other", "--tool", "codex"]);
    // fake claude/codex that announce if launched.
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = root.path().join("fakebin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    for (t, msg) in [("claude", "LAUNCHED-CLAUDE"), ("codex", "LAUNCHED-CODEX")] {
        let f = bin_dir.join(t);
        std::fs::write(&f, format!("#!/bin/sh\necho {msg}\n")).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // pick onlycodex (profile 1), then press 'c' (claude - NOT this profile's tool).
    child.stdin.as_mut().unwrap().write_all(b"1\nc\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(o.contains("x new codex"), "offers codex: {o}");
    assert!(
        !o.contains("c new claude"),
        "does NOT offer claude for a codex-only profile: {o}"
    );
    assert!(
        !o.contains("LAUNCHED-CLAUDE"),
        "pressing 'c' must not launch claude: {o}"
    );
}

// setup must not abort the whole wizard when ONE tool's login is unreadable:
// a corrupt Claude config must not stop Codex from being saved.
#[test]
fn setup_continues_past_an_unreadable_tool() {
    let root = tempfile::tempdir().unwrap();
    // Codex valid; Claude present() true but identity() errors - its PRIMARY
    // credential file is corrupt, so the login reads as present-but-unreadable.
    seed_codex(root.path(), "acct-A");
    std::fs::create_dir_all(root.path().join(".claude")).unwrap();
    std::fs::write(root.path().join(".claude/.credentials.json"), b"NOT JSON {").unwrap();
    let (o, c) = run_setup(root.path(), "codexname\nskip\n");
    assert_eq!(c, 0, "{o}");
    let (names, _e, _c) = run(root.path(), &["ls", "--names"]);
    assert!(
        names.contains("codexname"),
        "codex saved despite the claude error: {names} / {o}"
    );
}

// --- pre-switch running-session guard (SWAPDEX_TEST_CLAUDE_GUARD hook) ---

fn seed_two_claude_profiles(root: &Path) {
    // After this, live = B (b@x.com); alpha=A, beta=B are saved.
    seed_claude(root, "uuid-A", "a@x.com");
    run(root, &["add", "alpha", "--tool", "claude"]);
    seed_claude(root, "uuid-B", "b@x.com");
    run(root, &["add", "beta", "--tool", "claude"]);
}

#[test]
fn claude_switch_is_refused_when_a_session_holds_this_slot() {
    let root = tempfile::tempdir().unwrap();
    seed_two_claude_profiles(root.path());
    // A claude session (per the test hook) is running on this login slot.
    let (_o, e, c) = run_env(
        root.path(),
        &["use", "alpha", "--tool", "claude"],
        &[("SWAPDEX_TEST_CLAUDE_GUARD", "same-slot")],
    );
    assert_eq!(c, 1, "refused switch exits 1: {e}");
    assert!(
        e.contains("running on THIS login slot"),
        "explains the refusal: {e}"
    );
    // Nothing switched - the live login is still B, and no backup was written.
    let (o, _e, _c) = run(root.path(), &["status"]);
    assert!(o.contains("b@x.com"), "live login unchanged: {o}");
}

#[test]
fn claude_switch_force_overrides_the_running_session_guard() {
    let root = tempfile::tempdir().unwrap();
    seed_two_claude_profiles(root.path());
    let (o, e, c) = run_env(
        root.path(),
        &["use", "alpha", "--tool", "claude", "--force"],
        &[("SWAPDEX_TEST_CLAUDE_GUARD", "same-slot")],
    );
    assert_eq!(c, 0, "--force switches through the guard: {o}{e}");
    let (s, _e, _c) = run(root.path(), &["status"]);
    assert!(
        s.contains("a@x.com"),
        "switched to alpha despite the session: {s}"
    );
}

#[test]
fn claude_switch_fails_closed_when_a_running_session_slot_is_unknown() {
    let root = tempfile::tempdir().unwrap();
    seed_two_claude_profiles(root.path());
    let (_o, e, c) = run_env(
        root.path(),
        &["use", "alpha", "--tool", "claude"],
        &[("SWAPDEX_TEST_CLAUDE_GUARD", "unknown")],
    );
    assert_eq!(c, 1, "unknown slot fails closed: {e}");
    assert!(
        e.contains("could not read which login slot"),
        "fail-closed message: {e}"
    );
    let (o, _e, _c) = run(root.path(), &["status"]);
    assert!(o.contains("b@x.com"), "live login unchanged: {o}");
}

// Codex isolates an account with CODEX_HOME the same way Claude does with
// CLAUDE_CONFIG_DIR. Given a slot, switching Codex must repoint - never copy
// auth.json, which is the whole reason saved Codex logins went stale and the
// reason a running session could revoke the account being switched.
#[test]
fn switching_a_codex_slot_repoints_instead_of_copying() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(root.path(), "acct-LIVE");
    let live = root.path().join(".codex/auth.json");
    let before = std::fs::read(&live).unwrap();

    let (o, e, c) = run(
        root.path(),
        &["run", "other", "--tool", "codex", "--no-launch"],
    );
    assert_eq!(c, 0, "{o}{e}");
    let (o, e, c) = run(root.path(), &["use", "other", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");

    // The live login is untouched: no copy happened.
    assert_eq!(
        std::fs::read(&live).unwrap(),
        before,
        "switching a slot must not write the live auth.json"
    );
    // The pointer moved, and it is CODEX's own - Claude's default is separate.
    let store = root.path().join(".local/share/swapdex");
    let ptr = std::fs::read_to_string(store.join("active-codex")).expect("codex pointer");
    assert!(!ptr.trim().is_empty(), "pointer names the slot dir");
    assert!(
        !store.join("active-claude").exists(),
        "switching Codex must not move where Claude launches"
    );
}

// Moving the pointer is not moving a conversation. Without a proxy the change
// reaches the next launch only, and saying otherwise made an assistant report
// "this account serves you from the next turn" while the session carried on with
// the old one.
#[test]
fn a_slot_switch_says_whether_anything_running_actually_moved() {
    use swapdex::commands::switch_outcome_line;
    let without = switch_outcome_line("claude-code", "work", false, true);
    assert!(
        without.contains("NEXT claude you start"),
        "says when it applies: {without}"
    );
    assert!(
        without.contains("already open keeps the account"),
        "and what does NOT change: {without}"
    );
    assert!(
        without.contains("swapdex proxy"),
        "and the one step that would move it: {without}"
    );
    assert!(
        !without.contains("serves this session"),
        "never claims the running session moved: {without}"
    );

    let with = switch_outcome_line("claude-code", "work", true, true);
    assert!(
        with.contains("serves this session from the next turn"),
        "with a proxy it genuinely does: {with}"
    );

    // Shared history is the normal state now, so the switch says nothing about
    // conversations going out of view - that warning was true once and would
    // teach a rule the tool no longer follows.
    assert!(
        !without.contains("stay with the account"),
        "no stale warning once history is shared: {without}"
    );
    // An account that has NOT been repaired still loses sight of them, and is
    // told what fixes it. Silence there would be the older failure in reverse.
    let unshared = switch_outcome_line("claude-code", "work", false, false);
    assert!(
        unshared.contains("share-history"),
        "an un-repaired account is told how to fix it: {unshared}"
    );

    // Codex names its own proxy flag, so the fix is copy-pasteable.
    let codex = switch_outcome_line("codex", "work", false, true);
    assert!(codex.contains("NEXT codex you start"), "{codex}");
    assert!(codex.contains("swapdex proxy --tool codex"), "{codex}");
}

/// Renaming an account must rename it everywhere it is registered, whatever
/// tool it belongs to. It looked in Claude's registry only, so a Codex slot was
/// never found: the snapshot was renamed and the slot kept its old name, and
/// the machine then had one account answering to two - `ls` saying one thing
/// and the registry another.
#[test]
fn renaming_a_codex_slot_actually_renames_it() {
    let dir = tempfile::tempdir().unwrap();
    let paths = swapdex::paths::Paths::rooted(dir.path());
    let mut slots = swapdex::slots::Slots::open_for_update(&paths, "codex").unwrap();
    slots.create("A").unwrap();
    // `rename` takes the registry lock for itself, so give it back first.
    drop(slots);

    assert_eq!(
        swapdex::commands::rename(&paths, "A", "codex-main").unwrap(),
        0
    );

    let after = swapdex::slots::Slots::open_for(&paths, "codex").unwrap();
    assert!(
        after.get("codex-main").is_some(),
        "the slot took the new name"
    );
    assert!(
        after.get("A").is_none(),
        "and does not answer to the old one"
    );
}

/// A codex slot signed in as `account`, holding `token` as its access token.
fn seed_codex_slot(root: &Path, name: &str, account: &str, token: &str) {
    let dir = root.join(format!(".codex-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(
            r#"{{"tokens":{{"access_token":"{token}","account_id":"{account}","refresh_token":"R"}}}}"#
        ),
    )
    .unwrap();
    chmod600(&dir.join("auth.json"));
    let store = root.join(".local/share/swapdex");
    std::fs::create_dir_all(&store).unwrap();
    let file = store.join("slots.json");
    let mut list: Vec<serde_json::Value> = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    list.push(serde_json::json!({
        "name": name,
        "id": format!("id-{name}"),
        "config_dir": dir,
        "adopted": true,
        "tool": "codex",
    }));
    std::fs::write(&file, serde_json::to_vec(&list).unwrap()).unwrap();
}

/// A saved codex snapshot in the store - a COPY, which nothing refreshes.
fn seed_codex_snapshot(root: &Path, name: &str, account: &str, token: &str) {
    let d = root
        .join(".local/share/swapdex/accounts")
        .join(name)
        .join("codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("auth"),
        format!(
            r#"{{"tokens":{{"access_token":"{token}","account_id":"{account}","refresh_token":"R"}}}}"#
        ),
    )
    .unwrap();
    chmod600(&d.join("auth"));
}

/// A fake curl that answers the Codex usage endpoint for ONE live token.
fn write_fake_codex_curl(dir: &Path, live_token: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-codex-curl");
    std::fs::write(
        &p,
        format!(
            r#"#!/bin/sh
cfg=$(cat)
case "$cfg" in
  *"{live_token}"*) printf '{{"email":"a@x.com","plan_type":"pro","rate_limit":{{"primary_window":{{"used_percent":24.0,"limit_window_seconds":604800,"reset_at":1893456000}}}}}}\n200' ;;
  *) printf '{{"detail":"invalid token"}}\n401' ;;
esac
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const JWT_LAPSED: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjEwMDAwMDAwMDB9.sig";
const JWT_LIVE: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.sig";

/// A saved copy is not the account, and `quota` reported it as one.
///
/// Found on a real machine: one row printed "7d 100% left" and the row under
/// it printed "token rejected". They were the SAME login - the second was a
/// 42-day-old copy kept under another name. Nothing was wrong with that
/// account.
#[test]
fn a_stale_codex_copy_does_not_condemn_the_login_it_is_a_copy_of() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "live", "acct-1", JWT_LIVE);
    seed_codex_snapshot(root.path(), "copy", "acct-1", JWT_LAPSED);
    let curl = write_fake_codex_curl(root.path(), JWT_LIVE);
    let (o, e, c) = run_env(
        root.path(),
        &["quota"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("live"), "the live slot is listed: {o}");
    assert!(
        o.contains("copy of the account signed in as 'live'"),
        "the copy says whose account it copies: {o}"
    );
    assert!(
        !o.contains("token rejected"),
        "and never reports that account as rejected: {o}"
    );
    assert!(
        !o.contains("swapdex run copy"),
        "a copy is not renewed by running it: {o}"
    );
}

/// `--json` is where the Codex rows send the reader, and it had no Codex in it.
///
/// `codex_usage::Fetch::Unexpected` is documented "kept whole so `--json` can
/// show what came back", and a Codex row with no windows says "`swapdex quota
/// --json` to inspect" - but the JSON branch returned before Codex was ever
/// read, so a Codex-only machine reported `"accounts": []` and nothing else.
#[test]
fn quota_json_carries_the_codex_accounts_its_own_advice_points_at() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "live", "acct-1", JWT_LIVE);
    seed_codex_snapshot(root.path(), "copy", "acct-1", JWT_LAPSED);
    let curl = write_fake_codex_curl(root.path(), JWT_LIVE);
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    let codex = v["codex"]
        .as_array()
        .unwrap_or_else(|| panic!("a machine with Codex accounts lists them: {v}"));
    assert_eq!(codex.len(), 2, "both accounts, slot and copy: {v}");

    let live = codex.iter().find(|r| r["name"] == "live").unwrap();
    assert_eq!(live["status"], "ok", "{v}");
    assert_eq!(live["email"], "a@x.com");
    assert_eq!(live["plan"], "pro");
    assert_eq!(live["seven_day"]["used_pct"], 24.0, "{v}");

    // The copy is reported as the copy it is, not as a rejected account.
    let copy = codex.iter().find(|r| r["name"] == "copy").unwrap();
    assert_eq!(copy["status"], "expired", "{v}");
    assert_eq!(
        copy["copy_of"], "live",
        "and names the account it is a copy of: {v}"
    );
}

/// The other JSON path: a machine with BOTH kinds of account.
///
/// `quota --json` has two exits - one for a machine with no Claude accounts and
/// one for a machine with some - and a fix wired into either alone leaves the
/// other reporting a Codex account that is right there.
#[test]
fn quota_json_carries_codex_on_a_machine_that_also_has_claude() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "main", "--tool", "claude"]);
    seed_codex_slot(root.path(), "live", "acct-1", JWT_LIVE);
    // One fake curl answers both endpoints: the Claude token seeded above is
    // "AT", and the Codex slot's is a JWT.
    let curl = write_fake_both_curl(root.path(), JWT_LIVE);
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    assert_eq!(
        v["accounts"].as_array().map(Vec::len),
        Some(1),
        "the Claude account is still there: {v}"
    );
    let codex = v["codex"]
        .as_array()
        .unwrap_or_else(|| panic!("and the Codex account is too: {v}"));
    assert_eq!(codex.len(), 1, "{v}");
    assert_eq!(codex[0]["name"], "live");
    assert_eq!(codex[0]["status"], "ok", "{v}");
}

/// A fake curl answering BOTH usage endpoints, keyed on the bearer token.
fn write_fake_both_curl(dir: &Path, codex_token: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-both-curl");
    std::fs::write(
        &p,
        format!(
            r#"#!/bin/sh
cfg=$(cat)
case "$cfg" in
  *"{codex_token}"*) printf '{{"email":"a@x.com","plan_type":"pro","rate_limit":{{"primary_window":{{"used_percent":24.0,"limit_window_seconds":604800,"reset_at":1893456000}}}}}}\n200' ;;
  *"Bearer AT"*) printf '{{"five_hour":{{"utilization":25.0,"resets_at":1893456000}},"seven_day":{{"utilization":50.0}}}}\n200' ;;
  *) printf '{{"type":"error","error":{{"type":"authentication_error"}}}}\n401' ;;
esac
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A live Codex login at `~/.codex/auth.json` under the sandbox root.
/// Saving the account you already have, under a second name, says nothing.
///
/// `add --update` refuses to REPOINT a name at a different account, because
/// "that changes what the name means". The reverse - pointing a second NAME at
/// the same account - had no such check, so a machine ended up with two profiles
/// whose credentials were byte-identical, listed as two accounts. The listing
/// then reads as two logins to switch between when there is one.
#[test]
fn add_says_when_this_account_is_already_saved_under_another_name() {
    let root = tempfile::tempdir().unwrap();
    seed_live_codex(root.path(), "acct-1");
    let (out, _e, code) = run(root.path(), &["add", "work", "--tool", "codex"]);
    assert_eq!(code, 0, "the first save is ordinary: {out}");

    let (out, err, code) = run(root.path(), &["add", "personal", "--tool", "codex"]);
    assert_eq!(code, 0, "saving it twice is allowed, not refused");
    let both = format!("{out}{err}");
    assert!(
        both.contains("already saved as") && both.contains("work"),
        "and it names the profile that already holds this account: {both}"
    );
}

fn seed_live_codex(root: &Path, account: &str) {
    let d = root.join(".codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("auth.json"),
        format!(
            r#"{{"auth_mode":"chatgpt","last_refresh":"2026-09-01T00:00:00Z","tokens":{{"access_token":"{JWT_LIVE}","account_id":"{account}","refresh_token":"R"}}}}"#
        ),
    )
    .unwrap();
    chmod600(&d.join("auth.json"));
}

/// A saved Codex profile last refreshed `days_ago` days back.
fn seed_codex_profile_aged(root: &Path, name: &str, account: &str, days_ago: i64) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - days_ago * 86400;
    // RFC3339 of `secs`, built the long way so the test needs no date crate.
    let days = secs / 86400;
    let (mut y, mut d) = (1970i64, days);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    while d >= months[m] {
        d -= months[m];
        m += 1;
    }
    let stamp = format!("{y:04}-{:02}-{:02}T00:00:00Z", m + 1, d + 1);

    let dir = root
        .join(".local/share/swapdex/accounts")
        .join(name)
        .join("codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth"),
        format!(
            r#"{{"auth_mode":"chatgpt","last_refresh":"{stamp}","tokens":{{"access_token":"{JWT_LAPSED}","account_id":"{account}","refresh_token":"R"}}}}"#
        ),
    )
    .unwrap();
    chmod600(&dir.join("auth"));
}

/// Switching INTO an ancient login is the moment the reader can act on it.
///
/// `ls` marks a snapshot older than 30 days "stale" for all four tools, and the
/// switch itself warned for Claude only - so `use` installed a Codex login whose
/// refresh token may be long revoked and said nothing but "switched".
#[test]
fn switching_to_an_ancient_codex_login_says_it_is_old() {
    let root = tempfile::tempdir().unwrap();
    seed_live_codex(root.path(), "acct-live");
    seed_codex_profile_aged(root.path(), "old", "acct-old", 60);
    let (o, e, c) = run(root.path(), &["use", "old", "--tool", "codex"]);
    let all = format!("{o}{e}");
    assert_eq!(c, 0, "{all}");
    assert!(all.contains("switched codex"), "the switch happens: {all}");
    assert!(
        all.contains("old"),
        "and says the saved login is old: {all}"
    );
    assert!(
        all.contains("refresh token"),
        "naming what may have lapsed: {all}"
    );
}

/// The same switch to a FRESH saved login stays quiet - the note must not
/// become the noise the Claude rule was written to avoid.
#[test]
fn switching_to_a_recent_codex_login_stays_quiet() {
    let root = tempfile::tempdir().unwrap();
    seed_live_codex(root.path(), "acct-live");
    seed_codex_profile_aged(root.path(), "fresh", "acct-fresh", 1);
    let (o, e, c) = run(root.path(), &["use", "fresh", "--tool", "codex"]);
    let all = format!("{o}{e}");
    assert_eq!(c, 0, "{all}");
    assert!(all.contains("switched codex"), "{all}");
    assert!(
        !all.contains("refresh token"),
        "a day-old login is not old news: {all}"
    );
}

/// A fake curl that answers an OAuth refresh exchange with a new token.
fn write_fake_oauth_curl(dir: &Path, new_access: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-oauth-curl");
    std::fs::write(
        &p,
        format!(
            r#"#!/bin/sh
cat > /dev/null
printf '{{"access_token":"{new_access}","refresh_token":"R2","id_token":"ID2"}}\n200'
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// The unattended sweep is what keeps an idle account alive - and it swept
/// Claude only.
///
/// A Codex access token lives ten days and ONLY a Codex run renews it, so an
/// account nobody opens dies on a schedule. `swapdex refresh <name>` learned to
/// renew Codex; `refresh --keep-alive`, the half meant to run unattended, did
/// not, and reported "no Claude accounts to keep alive" on a machine whose Codex
/// account was days dead.
#[test]
fn the_keep_alive_sweep_renews_a_codex_account_too() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "work", "acct-1", JWT_LAPSED);
    let curl = write_fake_oauth_curl(root.path(), JWT_LIVE);
    let (o, e, c) = run_env(
        root.path(),
        &["refresh", "--keep-alive"],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
        ],
    );
    let all = format!("{o}{e}");
    assert_eq!(c, 0, "{all}");
    assert!(all.contains("renewed work"), "the sweep renews it: {all}");

    // And the renewal actually landed: the slot now holds the new token, and
    // the rotated refresh token replaced the spent one.
    let auth = std::fs::read_to_string(root.path().join(".codex-work/auth.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&auth).unwrap();
    assert_eq!(v["tokens"]["access_token"], JWT_LIVE, "{auth}");
    assert_eq!(
        v["tokens"]["refresh_token"], "R2",
        "the rotated refresh token replaced the spent one: {auth}"
    );
}

/// A Codex account with time left is not the sweep's business: every renewal
/// rotates the refresh token, and rotating one more than needed is the risk the
/// sweep exists to avoid, not to cause.
#[test]
fn the_keep_alive_sweep_leaves_a_current_codex_account_alone() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "work", "acct-1", JWT_LIVE);
    let curl = write_fake_oauth_curl(root.path(), "SHOULD-NOT-BE-WRITTEN");
    let (o, e, c) = run_env(
        root.path(),
        &["refresh", "--keep-alive"],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
        ],
    );
    let all = format!("{o}{e}");
    assert_eq!(c, 0, "{all}");
    assert!(!all.contains("renewed work"), "{all}");
    let auth = std::fs::read_to_string(root.path().join(".codex-work/auth.json")).unwrap();
    assert!(auth.contains(JWT_LIVE), "its token is untouched: {auth}");
}

/// The first screen a new user sees counted Claude accounts only.
///
/// swapdex learned this once already: `quota` carries the note "a machine can
/// hold only Codex accounts, and returning here left it with nothing to show but
/// a note about a tool it does not use". Onboarding still ended with "No
/// accounts yet. Log into Claude" on a machine whose Codex account was
/// registered and working.
#[test]
fn onboarding_a_codex_only_machine_does_not_say_there_are_no_accounts() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "work", "acct-1", JWT_LIVE);
    let (o, e, c) = run(root.path(), &["onboard"]);
    let all = format!("{o}{e}");
    assert_eq!(c, 0, "{all}");
    assert!(
        !all.contains("No accounts yet"),
        "this machine has one: {all}"
    );
    assert!(
        !all.contains("Log into Claude"),
        "and it is not a Claude account: {all}"
    );
}

/// A machine with nothing still gets told what to do - and told it for both
/// tools, since either one can be the only one someone uses.
#[test]
fn onboarding_an_empty_machine_names_both_tools() {
    let root = tempfile::tempdir().unwrap();
    let (o, e, c) = run(root.path(), &["onboard"]);
    let all = format!("{o}{e}");
    assert_eq!(c, 0, "{all}");
    assert!(all.contains("No accounts yet"), "{all}");
    assert!(all.contains("codex"), "Codex is a way in too: {all}");
}

fn registered_slot_names(root: &Path, tool: &str) -> Vec<String> {
    std::fs::read(root.join(".local/share/swapdex/slots.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<serde_json::Value>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|record| record["tool"].as_str().unwrap_or("claude-code") == tool)
        .filter_map(|record| record["name"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn migrate_claude_creates_a_slot_for_an_unmatched_account() {
    let root = tempfile::tempdir().unwrap();
    seed_claude_profile(root.path(), "claude-new", "uuid-new", "new@x.com", "AT");

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "claude"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("Claude Code:"), "grouped by tool: {o}");
    assert!(o.contains("Created slots for: claude-new"), "{o}");
    assert!(
        o.contains("swapdex run claude-new --tool claude"),
        "names the one-time sign-in: {o}"
    );
    assert_eq!(
        registered_slot_names(root.path(), "claude-code"),
        vec!["claude-new"]
    );
}

#[test]
fn migrate_codex_creates_a_slot_for_an_unmatched_account() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_snapshot(root.path(), "codex-new", "acct-new", "AT");

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("Codex:"), "grouped by tool: {o}");
    assert!(o.contains("Created slots for: codex-new"), "{o}");
    assert!(
        o.contains("swapdex run codex-new --tool codex"),
        "names the one-time sign-in: {o}"
    );
    assert_eq!(
        registered_slot_names(root.path(), "codex"),
        vec!["codex-new"]
    );
}

#[test]
fn migrate_claude_reports_a_differently_named_slot_copy_without_creating() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(
        root.path(),
        "claude-main",
        &root.path().join("slot-claude-main"),
        "AT",
        "main@x.com",
    );
    seed_claude_profile(
        root.path(),
        "legacy-claude",
        "u-claude-main",
        "main@x.com",
        "OLD",
    );

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "claude"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(
        o.contains("saved copy of slot 'claude-main'"),
        "names the matching slot: {o}"
    );
    // The flag spelling `--tool` actually takes, the way every other remedy in
    // the tool spells it (`pretty_tool_flag`). This used to read `claude-code`
    // back when that was the only spelling `rm --tool` accepted.
    assert!(
        o.contains("swapdex rm legacy-claude --tool claude"),
        "gives the optional cleanup command: {o}"
    );
    assert_eq!(
        registered_slot_names(root.path(), "claude-code"),
        vec!["claude-main"]
    );
}

#[test]
fn migrate_codex_reports_a_differently_named_slot_copy_without_creating() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "codex-main", "acct-main", "AT");
    seed_codex_snapshot(root.path(), "legacy-codex", "acct-main", "OLD");

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(
        o.contains("saved copy of slot 'codex-main'"),
        "names the matching slot: {o}"
    );
    assert!(
        o.contains("swapdex rm legacy-codex --tool codex"),
        "gives the optional cleanup command: {o}"
    );
    assert_eq!(
        registered_slot_names(root.path(), "codex"),
        vec!["codex-main"]
    );
}

#[test]
fn migrate_codex_tool_home_profile_is_recognized_as_a_slot_copy() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "codex-main", "dcf87e80", "AT");
    seed_codex_snapshot(root.path(), "codex", "dcf87e80", "OLD");

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("saved copy of slot 'codex-main'"), "{o}");
    assert!(!o.contains("would read as the tool's own home"), "{o}");
    assert_eq!(
        registered_slot_names(root.path(), "codex"),
        vec!["codex-main"]
    );
}

#[test]
fn migrate_claude_is_silent_for_a_profile_already_slotted_by_name_and_account() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(
        root.path(),
        "same-claude",
        &root.path().join("slot-same-claude"),
        "AT",
        "same@x.com",
    );
    seed_claude_profile(
        root.path(),
        "same-claude",
        "u-same-claude",
        "same@x.com",
        "OLD",
    );

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "claude"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("Claude Code:\n  Nothing to migrate."), "{o}");
    assert!(
        !o.contains("same-claude"),
        "already slotted stays silent: {o}"
    );
}

#[test]
fn migrate_codex_is_silent_for_a_profile_already_slotted_by_name_and_account() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "same-codex", "acct-same", "AT");
    seed_codex_snapshot(root.path(), "same-codex", "acct-same", "OLD");

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("Codex:\n  Nothing to migrate."), "{o}");
    assert!(
        !o.contains("same-codex"),
        "already slotted stays silent: {o}"
    );
}

#[test]
fn migrate_claude_reports_an_unreadable_snapshot_without_creating() {
    let root = tempfile::tempdir().unwrap();
    seed_claude_profile(root.path(), "broken-claude", "uuid-bad", "bad@x.com", "AT");
    std::fs::write(
        root.path()
            .join(".local/share/swapdex/accounts/broken-claude/claude-code/oauth_account"),
        b"not json",
    )
    .unwrap();

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "claude"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(
        o.contains("Could not read the account ID from saved profile 'broken-claude'"),
        "{o}"
    );
    assert!(registered_slot_names(root.path(), "claude-code").is_empty());
}

#[test]
fn migrate_codex_reports_an_unreadable_snapshot_without_creating() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_snapshot(root.path(), "broken-codex", "acct-bad", "AT");
    std::fs::write(
        root.path()
            .join(".local/share/swapdex/accounts/broken-codex/codex/auth"),
        b"not json",
    )
    .unwrap();

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(
        o.contains("Could not read the account ID from saved profile 'broken-codex'"),
        "{o}"
    );
    assert!(registered_slot_names(root.path(), "codex").is_empty());
}

#[test]
fn migrate_tool_codex_leaves_claude_profiles_untouched() {
    let root = tempfile::tempdir().unwrap();
    seed_claude_profile(root.path(), "only-claude", "uuid-only", "only@x.com", "AT");
    seed_codex_snapshot(root.path(), "only-codex", "acct-only", "AT");

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("Codex:"), "{o}");
    assert!(!o.contains("Claude Code:"), "scope excludes Claude: {o}");
    assert!(
        !o.contains("only-claude"),
        "scope excludes its profile: {o}"
    );
    assert_eq!(
        registered_slot_names(root.path(), "codex"),
        vec!["only-codex"]
    );
    assert!(registered_slot_names(root.path(), "claude-code").is_empty());
}

#[test]
fn migrate_tool_flag_offers_only_slot_capable_tools() {
    let root = tempfile::tempdir().unwrap();
    let (o, e, c) = run(root.path(), &["migrate", "--help"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("possible values: claude, codex"), "{o}");
    assert!(!o.contains("gemini") && !o.contains("antigravity"), "{o}");
    assert!(!o.contains("all:"), "{o}");
}

#[test]
fn migrate_defaults_to_both_tools_and_orders_each_tool_categories() {
    let root = tempfile::tempdir().unwrap();
    seed_claude_profile(root.path(), "claude-new", "uuid-new", "new@x.com", "AT");
    seed_codex_snapshot(root.path(), "codex-new", "acct-new", "AT");
    seed_codex_slot(root.path(), "codex-main", "acct-main", "AT");
    seed_codex_snapshot(root.path(), "codex-copy", "acct-main", "OLD");
    seed_codex_snapshot(root.path(), "codex-broken", "acct-bad", "AT");
    std::fs::write(
        root.path()
            .join(".local/share/swapdex/accounts/codex-broken/codex/auth"),
        b"not json",
    )
    .unwrap();

    let (o, e, c) = run(root.path(), &["migrate"]);
    assert_eq!(c, 0, "{o}{e}");
    let claude = o.find("Claude Code:").expect("Claude group");
    let codex = o.find("Codex:").expect("Codex group");
    assert!(claude < codex, "stable tool grouping: {o}");
    let created = o.find("Created slots for: codex-new").expect("case C");
    let copy = o.find("saved copy of slot 'codex-main'").expect("case B");
    let unreadable = o
        .find("account ID from saved profile 'codex-broken'")
        .expect("case D");
    assert!(
        created < copy && copy < unreadable,
        "C, then B, then D: {o}"
    );
    assert_eq!(
        registered_slot_names(root.path(), "claude-code"),
        vec!["claude-new"]
    );
    assert_eq!(
        registered_slot_names(root.path(), "codex"),
        vec!["codex-main", "codex-new"]
    );
}

#[test]
fn migrate_does_not_duplicate_a_slot_that_is_waiting_for_sign_in() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_snapshot(root.path(), "pending", "acct-pending", "AT");

    let (first, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{first}{e}");
    let (second, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{second}{e}");
    assert!(second.contains("Nothing to migrate"), "{second}");
    assert_eq!(
        registered_slot_names(root.path(), "codex"),
        vec!["pending"],
        "an unsigned migrated slot must remain the only slot"
    );
}

#[test]
fn migrate_creates_one_slot_for_two_saved_profiles_of_one_account() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_snapshot(root.path(), "first-copy", "acct-shared", "AT-1");
    seed_codex_snapshot(root.path(), "second-copy", "acct-shared", "AT-2");

    let (o, e, c) = run(root.path(), &["migrate", "--tool", "codex"]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(o.contains("Created slots for: first-copy"), "{o}");
    assert!(o.contains("saved copy of slot 'first-copy'"), "{o}");
    assert_eq!(
        registered_slot_names(root.path(), "codex"),
        vec!["first-copy"],
        "one account must require only one fresh sign-in"
    );
}

#[test]
fn onboard_counts_only_profiles_that_need_slots_and_mentions_copies_separately() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_snapshot(root.path(), "needs-slot", "acct-new", "AT");
    seed_codex_slot(root.path(), "codex-main", "acct-main", "AT");
    seed_codex_snapshot(root.path(), "saved-copy", "acct-main", "OLD");

    let (o, e, c) = run_env(root.path(), &["onboard"], &[("SWAPDEX_ASSUME_TTY", "1")]);
    assert_eq!(c, 0, "{o}{e}");
    assert!(
        o.contains("You have 1 saved"),
        "only case C is counted: {o}"
    );
    assert!(
        o.contains("1 saved profile copy") && o.contains("differently named slot"),
        "case B gets its own sentence: {o}"
    );
}

/// `serve` changes who pays, and it wrote that down without taking the lock.
///
/// The switch timeline is the only record of which account a turn was billed
/// to, and it is read-modify-written whole: two writers that do not both ask
/// for the store lock lose one of the two events. `use` and `restore` take it
/// before writing, and `login` retries for it rather than write unlocked -
/// its own comment names that as the bug it fixed. `serve` took nothing.
#[test]
fn serve_takes_the_store_lock_the_other_payer_writers_take() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "u-a", "a@x.com");
    run(root.path(), &["add", "pa", "--tool", "claude"]);
    let store = root.path().join(".local/share/swapdex");
    seed_slot(
        root.path(),
        "work",
        &store.join("slots/work"),
        "AT2",
        "w@x.com",
    );
    let timeline = store.join("timeline.jsonl");
    std::fs::remove_file(&timeline).ok();

    // Make the lock impossible to take while leaving the rest of the store
    // writable, so a command that skips the lock still gets its write through.
    let lock = store.join(".lock");
    std::fs::remove_file(&lock).ok();
    std::fs::create_dir(&lock).unwrap();

    let (_, _, code) = run(root.path(), &["use", "pa", "--tool", "claude"]);
    assert_eq!(code, 4, "`use` refuses when the store lock cannot be taken");

    let (o, e, code) = run(root.path(), &["serve", "work", "--tool", "claude"]);
    assert_eq!(
        code, 4,
        "and so must `serve`, which writes the same record: {o}{e}"
    );
    assert!(
        !timeline.exists(),
        "nothing may reach the timeline unlocked: {}",
        std::fs::read_to_string(&timeline).unwrap_or_default()
    );
}

/// A payer change that could not be recorded is not a success.
///
/// `serve` printed "now work" and exited 0 whether or not the event reached the
/// timeline, so the pointer could move with nothing saying when. The slot
/// pointer write on the line above already raises its failures.
#[test]
fn serve_does_not_report_success_when_the_payer_record_could_not_be_written() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "u-a", "a@x.com");
    run(root.path(), &["add", "pa", "--tool", "claude"]);
    let store = root.path().join(".local/share/swapdex");
    seed_slot(
        root.path(),
        "work",
        &store.join("slots/work"),
        "AT2",
        "w@x.com",
    );
    // The timeline cannot be written, while everything else still can.
    let timeline = store.join("timeline.jsonl");
    std::fs::remove_file(&timeline).ok();
    std::fs::create_dir(&timeline).unwrap();

    let (o, e, code) = run(root.path(), &["serve", "work", "--tool", "claude"]);
    assert_ne!(code, 0, "the failure must reach the exit code: {o}{e}");
}

/// A slot switch is a switch, and it left no record of itself.
///
/// Slots share one conversation store, so nothing in a transcript says which
/// account produced it - `usage` credits every token by the switch timeline
/// alone. `use <profile>` writes an event there. `use <slot>` - and every
/// account `setup` makes is a slot - wrote nothing, so the tokens that followed
/// went on being credited to whoever the previous event named.
#[test]
fn a_slot_switch_is_recorded_the_way_a_profile_switch_is() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join(".local/share/swapdex");
    seed_slot(
        root.path(),
        "work",
        &store.join("slots/work"),
        "AT-W",
        "w@x.com",
    );
    seed_slot(
        root.path(),
        "personal",
        &store.join("slots/personal"),
        "AT-P",
        "p@x.com",
    );

    let (o, e, c) = run(root.path(), &["use", "work", "--tool", "claude"]);
    assert_eq!(c, 0, "{o}{e}");
    let (o, e, c) = run(root.path(), &["use", "personal", "--tool", "claude"]);
    assert_eq!(c, 0, "{o}{e}");

    let timeline = std::fs::read_to_string(store.join("timeline.jsonl")).unwrap_or_default();
    let named: Vec<&str> = timeline
        .lines()
        .filter(|l| l.contains("claude-code"))
        .collect();
    assert_eq!(
        named.len(),
        2,
        "both slot switches are on the record: {timeline:?}"
    );
    assert!(
        named[0].contains("\"work\"") && named[1].contains("\"personal\""),
        "in the order they happened: {named:?}"
    );
}

/// `quota` must call the account the pointer serves the active one.
///
/// Whether a row is active was decided by comparing each slot's uuid against
/// the login in the tool's own config dir. A slot switch never writes that dir,
/// so on a machine whose accounts live only as slots there was no live uuid to
/// match and every row came back inactive - including the one paying for every
/// turn. The label lost its `(active)` marker, and a rejected token was
/// explained as a dead snapshot with `swapdex use <name>` as the cure, which
/// for a slot only moves a pointer and refreshes nothing.
#[test]
fn quota_marks_the_pointed_at_slot_active() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("slot-work");
    seed_slot(root.path(), "work", &dir, "AT", "work@x.com");
    let (o, e, c) = run(root.path(), &["use", "work", "--tool", "claude"]);
    assert_eq!(c, 0, "use failed:\n{o}{e}");

    let curl = write_fake_curl(root.path());
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    let accounts = v["accounts"].as_array().unwrap();
    let row = accounts
        .iter()
        .find(|a| a["name"] == "work")
        .unwrap_or_else(|| panic!("no row for work: {v}"));
    assert_eq!(
        row["active"],
        serde_json::json!(true),
        "quota does not call the pointed-at slot active: {v}"
    );
}

/// The marker must still follow the live login where no pointer exists.
///
/// A copy-model machine has no slot pointer and its live dir IS the answer.
/// Preferring the pointer must not cost it the marker, or the fix would be the
/// same defect facing the other way.
#[test]
fn quota_still_marks_the_live_login_active_without_a_pointer() {
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "uuid-A", "a@x.com");
    run(root.path(), &["add", "main", "--tool", "claude"]);

    let curl = write_fake_curl(root.path());
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    let accounts = v["accounts"].as_array().unwrap();
    let row = accounts
        .iter()
        .find(|a| a["name"] == "main")
        .unwrap_or_else(|| panic!("no row for main: {v}"));
    assert_eq!(
        row["active"],
        serde_json::json!(true),
        "quota lost the active marker on a machine with no slots: {v}"
    );
}

/// Exactly the pointed-at slot is active, not every slot.
///
/// The marker is what tells a reader which account the numbers below belong to
/// and which one a rejected token is a problem for. Two slots and one pointer
/// is the ordinary shape of this tool, so a check that answered "a slot exists"
/// rather than "this slot is the one" would mark both and say nothing at all.
#[test]
fn quota_marks_only_the_pointed_at_slot_active() {
    let root = tempfile::tempdir().unwrap();
    let one = root.path().join("slot-one");
    let two = root.path().join("slot-two");
    seed_slot(root.path(), "one", &one, "AT", "one@x.com");
    seed_slot(root.path(), "two", &two, "AT", "two@x.com");
    let (o, e, c) = run(root.path(), &["use", "two", "--tool", "claude"]);
    assert_eq!(c, 0, "use failed:\n{o}{e}");

    let curl = write_fake_curl(root.path());
    let (o, e, c) = run_env(
        root.path(),
        &["quota", "--json"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    assert_eq!(c, 0, "{o}{e}");
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    let accounts = v["accounts"].as_array().unwrap();
    let active: Vec<&str> = accounts
        .iter()
        .filter(|a| a["active"] == serde_json::json!(true))
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert_eq!(
        active,
        vec!["two"],
        "the pointer names 'two' and exactly one row should be active: {v}"
    );
}
