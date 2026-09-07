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
        vec!["rm", "home", "--yes"],
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

/// `SWAPDEX_ROOT` must contain every file a command writes.
///
/// `slash` asked `dirs` for the home directory and wrote
/// `~/.claude/commands/swap.md` and `~/.codex/skills/swap/SKILL.md` into the
/// REAL one - so running it under a test root touched the developer's own
/// assistants. The dirs belong to the assistants; the HOME they hang off is
/// still the one `paths` names.
#[test]
fn slash_writes_inside_the_sandbox_only() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let real_claude = dirs::home_dir().map(|h| h.join(".claude/commands/swap.md"));
    let before = real_claude.as_ref().and_then(|p| std::fs::metadata(p).ok());

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_swapdex"))
        .arg("slash")
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        root.join(".claude/commands/swap.md").exists(),
        "it must write under the sandbox root"
    );
    assert!(root.join(".codex/skills/swap/SKILL.md").exists());

    // And the real home is exactly as it was - still absent, or untouched.
    if let Some(p) = real_claude {
        match (before, std::fs::metadata(&p).ok()) {
            (None, after) => assert!(
                after.is_none(),
                "it created {} outside the sandbox",
                p.display()
            ),
            (Some(b), Some(a)) => assert_eq!(
                b.modified().ok(),
                a.modified().ok(),
                "it rewrote {} outside the sandbox",
                p.display()
            ),
            (Some(_), None) => panic!("it removed {}", p.display()),
        }
    }
}
