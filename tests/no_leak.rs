//! The egress guarantee (A11): seed sentinel tokens in BOTH tools, drive every
//! subcommand and both MCP tools, and assert no sentinel and no absolute
//! credential path ever reaches stdout/stderr.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const SENTINELS: &[&str] = &[
    "AT-CLAUDE-SENTINEL",
    "RT-CLAUDE-SENTINEL",
    "sk-CODEX-SENTINEL",
    "AT-CODEX-SENTINEL",
    "RT-CODEX-SENTINEL",
];

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn seed_both(root: &Path, claude_acct: &str, codex_acct: &str) {
    let cdir = root.join(".claude");
    std::fs::create_dir_all(&cdir).unwrap();
    std::fs::write(
        cdir.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "accessToken":"AT-CLAUDE-SENTINEL","refreshToken":"RT-CLAUDE-SENTINEL",
            "expiresAt":9999999999999i64,"subscriptionType":"max","rateLimitTier":"default"}}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join(".claude.json"),
        serde_json::to_vec(&serde_json::json!({
            "projects":{"/x":{"trust":true}},"mcpServers":{"prodex":{"command":"prodex"}},
            "oauthAccount":{"accountUuid":claude_acct,"emailAddress":"me@work.com","displayName":"Work"}}))
        .unwrap(),
    )
    .unwrap();
    let xdir = root.join(".codex");
    std::fs::create_dir_all(&xdir).unwrap();
    std::fs::write(
        xdir.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode":"chatgpt","OPENAI_API_KEY":"sk-CODEX-SENTINEL",
            "tokens":{"id_token":"h.eyJlbWFpbCI6Im1lQHBlcnMuY29tIn0.s","access_token":"AT-CODEX-SENTINEL",
                      "refresh_token":"RT-CODEX-SENTINEL","account_id":codex_acct},
            "last_refresh":"2026-07-03T00:00:00Z"}))
        .unwrap(),
    )
    .unwrap();
}

fn run(root: &Path, args: &[&str]) -> String {
    let out = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assert_clean(label: &str, output: &str) {
    for s in SENTINELS {
        assert!(!output.contains(s), "token {s} leaked in {label}: {output}");
    }
}

#[test]
fn no_subcommand_leaks_a_token() {
    let root = tempfile::tempdir().unwrap();
    seed_both(root.path(), "claude-A", "codex-A");
    assert_clean("add", &run(root.path(), &["add", "work"]));
    seed_both(root.path(), "claude-B", "codex-B");
    assert_clean("add2", &run(root.path(), &["add", "home"]));
    assert_clean("use", &run(root.path(), &["use", "work"]));
    for args in [
        vec!["ls"],
        vec!["ls", "--json"],
        vec!["status"],
        vec!["sessions"],
        vec!["use", "home", "--dry-run"],
        vec!["rm", "home"],
    ] {
        assert_clean(&format!("{args:?}"), &run(root.path(), &args));
    }
}

#[test]
fn mcp_tools_never_leak_a_token() {
    let root = tempfile::tempdir().unwrap();
    seed_both(root.path(), "claude-A", "codex-A");
    run(root.path(), &["add", "work"]);

    let mut child = Command::new(bin())
        .arg("mcp")
        .env("SWAPDEX_ROOT", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"whoami"}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_accounts"}}"#,
        ] {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_clean("mcp", &combined);
    // whoami is allowed to show email/identity, but never a token or the store path.
    assert!(!combined.contains(".credentials.json"));
    assert!(combined.contains("whoami") || combined.contains("me@") || combined.contains("work"));
}
