use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const STATUS_FILE: &str = ".swapdex-refresh-status.json";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn jwt(exp: i64) -> String {
    use base64::Engine;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!(
        "{}.{}.signature",
        encode(br#"{"alg":"none"}"#),
        encode(format!(r#"{{"exp":{exp}}}"#).as_bytes())
    )
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn codex_auth(account: &str, refresh: &str, access: &str) -> Vec<u8> {
    serde_json::to_vec_pretty(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "account_id": account,
            "refresh_token": refresh,
            "access_token": access,
            "id_token": "header.synthetic-identity.signature"
        },
        "last_refresh": "2026-09-01T00:00:00Z"
    }))
    .unwrap()
}

fn seed_slots(root: &Path, specs: &[(&str, &str, &str)]) -> Vec<PathBuf> {
    let store = root.join(".local/share/swapdex");
    let mut records = Vec::new();
    let mut dirs = Vec::new();
    for (name, id, tool) in specs {
        let dir = store.join("slots").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        records.push(serde_json::json!({
            "name": name,
            "id": id,
            "config_dir": dir,
            "adopted": false,
            "tool": tool
        }));
        dirs.push(dir);
    }
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec(&records).unwrap(),
    )
    .unwrap();
    dirs
}

fn seed_codex(root: &Path, name: &str, account: &str, refresh: &str, exp: i64) -> PathBuf {
    let dirs = seed_slots(root, &[(name, "codex-slot", "codex")]);
    let dir = dirs[0].clone();
    std::fs::write(
        dir.join("auth.json"),
        codex_auth(account, refresh, &jwt(exp)),
    )
    .unwrap();
    dir
}

fn fake_curl(root: &Path) -> PathBuf {
    let script = root.join("fake-curl");
    std::fs::write(
        &script,
        br#"#!/bin/sh
cat >/dev/null
if [ -n "$FAKE_REPLACE_FROM" ]; then
    cp "$FAKE_REPLACE_FROM" "$FAKE_AUTH_PATH" || exit 91
fi
if [ "${FAKE_EXIT:-0}" -ne 0 ]; then
    printf '%s\n' 'synthetic transport failure' >&2
    exit "$FAKE_EXIT"
fi
if [ -n "$FAKE_BODY" ]; then
    body=$FAKE_BODY
else
    body='{"error":"invalid_grant","detail":"server-body-sentinel"}'
fi
printf '%s\n%s' "$body" "${FAKE_STATUS:-401}"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();
    script
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("HOME", root)
        .output()
        .unwrap()
}

fn run_with_curl(root: &Path, curl: &Path, args: &[&str], vars: &[(&str, &str)]) -> Output {
    let mut command = Command::new(bin());
    command
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("HOME", root)
        .env("SWAPDEX_CURL", curl);
    for (name, value) in vars {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn combined(output: &Output) -> String {
    stdout(output) + &String::from_utf8_lossy(&output.stderr)
}

fn json_row(root: &Path, name: &str) -> serde_json::Value {
    let output = run(root, &["ls", "--json"]);
    assert!(output.status.success(), "{}", combined(&output));
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {}", stdout(&output)))
        .clone()
}

#[test]
fn expired_codex_slot_is_visible_in_human_and_json_ls() {
    let root = tempfile::tempdir().unwrap();
    seed_codex(
        root.path(),
        "codex-only",
        "account-one",
        "refresh-one",
        now_secs() - 60,
    );

    let human = run(root.path(), &["ls"]);
    let said = combined(&human);
    assert!(human.status.success(), "{said}");
    assert!(said.contains("codex expired"), "{said}");
    assert!(said.contains("needs refresh"), "{said}");

    let row = json_row(root.path(), "codex-only");
    let warning = row["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("codex expired"), "{row}");
    assert!(warning.contains("needs refresh"), "{row}");
}

#[test]
fn definitive_refresh_rejection_persists_into_later_listings() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "rejected",
        "account-rejected",
        "refresh-rejected",
        now_secs() - 60,
    );
    let before = std::fs::read(slot.join("auth.json")).unwrap();
    let curl = fake_curl(root.path());

    let refresh = run_with_curl(root.path(), &curl, &["refresh", "rejected"], &[]);
    let refresh_text = combined(&refresh);
    assert!(refresh.status.success(), "{refresh_text}");
    assert!(refresh_text.contains("idle too long"), "{refresh_text}");
    assert!(
        !refresh_text.contains("server-body-sentinel"),
        "{refresh_text}"
    );
    assert_eq!(
        std::fs::read(slot.join("auth.json")).unwrap(),
        before,
        "health evidence must not rewrite auth after failure"
    );

    let human = run(root.path(), &["ls"]);
    let said = combined(&human);
    assert!(said.contains("codex refresh rejected"), "{said}");
    assert!(said.contains("re-login required"), "{said}");

    let row = json_row(root.path(), "rejected");
    assert!(
        row["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("codex refresh rejected")),
        "{row}"
    );
    assert!(
        row["refresh_rejected_at_ms"]["codex"].is_number(),
        "rejection time should be machine-readable: {row}"
    );
}

#[test]
fn every_definitive_oauth_status_persists_rejection() {
    for status in ["400", "401", "403"] {
        let root = tempfile::tempdir().unwrap();
        let slot = seed_codex(
            root.path(),
            "rejected",
            "account-rejected",
            "refresh-rejected",
            now_secs() - 60,
        );
        let curl = fake_curl(root.path());
        let refresh = run_with_curl(
            root.path(),
            &curl,
            &["refresh", "rejected"],
            &[("FAKE_STATUS", status)],
        );
        assert!(refresh.status.success(), "{}", combined(&refresh));
        assert!(slot.join(STATUS_FILE).is_file(), "HTTP {status}");
        let row = json_row(root.path(), "rejected");
        assert!(
            row["warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("refresh rejected")),
            "HTTP {status}: {row}"
        );
    }
}

#[test]
fn successful_refresh_clears_matching_old_rejection() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "recovers",
        "account-recovers",
        "refresh-recovers",
        now_secs() - 60,
    );
    let curl = fake_curl(root.path());
    let rejected = run_with_curl(root.path(), &curl, &["refresh", "recovers"], &[]);
    assert!(rejected.status.success(), "{}", combined(&rejected));
    assert!(slot.join(STATUS_FILE).is_file());

    let response = serde_json::json!({
        "access_token": jwt(now_secs() + 86_400),
        "id_token": "header.recovered-identity.signature"
    })
    .to_string();
    let renewed = run_with_curl(
        root.path(),
        &curl,
        &["refresh", "recovers"],
        &[("FAKE_STATUS", "200"), ("FAKE_BODY", response.as_str())],
    );
    assert!(renewed.status.success(), "{}", combined(&renewed));
    assert!(!slot.join(STATUS_FILE).exists());
    let row = json_row(root.path(), "recovers");
    assert_eq!(row["warning"], serde_json::Value::Null, "{row}");
}

#[test]
fn transient_and_malformed_failures_do_not_become_permanent_rejections() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "transient",
        "account-transient",
        "refresh-transient",
        now_secs() - 60,
    );
    let curl = fake_curl(root.path());
    let cases = [
        vec![("FAKE_EXIT", "7")],
        vec![("FAKE_STATUS", "429")],
        vec![("FAKE_STATUS", "500")],
        vec![("FAKE_STATUS", "200"), ("FAKE_BODY", "not-json")],
    ];

    for vars in cases {
        let refresh = run_with_curl(root.path(), &curl, &["refresh", "transient"], &vars);
        assert!(refresh.status.success(), "{}", combined(&refresh));
        assert!(
            !slot.join(STATUS_FILE).exists(),
            "{vars:?} left permanent rejection evidence"
        );
        let row = json_row(root.path(), "transient");
        assert!(
            row["warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("codex expired")),
            "{vars:?}: {row}"
        );
        assert!(
            !row["warning"]
                .as_str()
                .unwrap_or_default()
                .contains("refresh rejected"),
            "{vars:?}: {row}"
        );
    }
}

#[test]
fn rejection_follows_account_and_refresh_token_but_not_access_token() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "binding",
        "account-old",
        "refresh-old",
        now_secs() - 60,
    );
    let curl = fake_curl(root.path());
    let refresh = run_with_curl(root.path(), &curl, &["refresh", "binding"], &[]);
    assert!(refresh.status.success(), "{}", combined(&refresh));

    std::fs::write(
        slot.join("auth.json"),
        codex_auth("account-old", "refresh-old", &jwt(now_secs() + 86_400)),
    )
    .unwrap();
    let access_only = json_row(root.path(), "binding");
    assert!(
        access_only["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("refresh rejected")),
        "access-only update hid matching evidence: {access_only}"
    );

    std::fs::write(
        slot.join("auth.json"),
        codex_auth("account-old", "refresh-new", &jwt(now_secs() + 86_400)),
    )
    .unwrap();
    let new_refresh = json_row(root.path(), "binding");
    assert_eq!(
        new_refresh["warning"],
        serde_json::Value::Null,
        "{new_refresh}"
    );

    std::fs::write(
        slot.join("auth.json"),
        codex_auth("account-old", "refresh-old", &jwt(now_secs() - 60)),
    )
    .unwrap();
    let second = run_with_curl(root.path(), &curl, &["refresh", "binding"], &[]);
    assert!(second.status.success(), "{}", combined(&second));
    std::fs::write(
        slot.join("auth.json"),
        codex_auth("account-new", "refresh-old", &jwt(now_secs() + 86_400)),
    )
    .unwrap();
    let new_account = json_row(root.path(), "binding");
    assert_eq!(
        new_account["warning"],
        serde_json::Value::Null,
        "{new_account}"
    );
}

#[test]
fn same_name_claude_and_codex_slots_report_the_correct_tool() {
    let root = tempfile::tempdir().unwrap();
    let dirs = seed_slots(
        root.path(),
        &[
            ("shared", "claude-slot", "claude-code"),
            ("shared", "codex-slot", "codex"),
        ],
    );
    std::fs::write(
        dirs[0].join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"claude-access","refreshToken":"claude-refresh","expiresAt":32503680000000}}"#,
    )
    .unwrap();
    std::fs::write(
        dirs[0].join(".claude.json"),
        br#"{"oauthAccount":{"accountUuid":"claude-account","emailAddress":"claude@example.com"}}"#,
    )
    .unwrap();
    std::fs::write(
        dirs[1].join("auth.json"),
        codex_auth("codex-account", "codex-refresh", &jwt(now_secs() - 60)),
    )
    .unwrap();
    let curl = fake_curl(root.path());

    let refresh = run_with_curl(root.path(), &curl, &["refresh", "shared"], &[]);
    assert!(refresh.status.success(), "{}", combined(&refresh));
    let row = json_row(root.path(), "shared");
    let warning = row["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("codex refresh rejected"), "{row}");
    assert!(!warning.contains("claude-code expired"), "{row}");
    assert!(row["refresh_rejected_at_ms"]["codex"].is_number(), "{row}");
}

#[test]
fn request_failure_cannot_mark_credentials_replaced_during_the_request() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "race",
        "account-old",
        "refresh-old",
        now_secs() - 60,
    );
    let replacement = root.path().join("replacement-auth.json");
    std::fs::write(
        &replacement,
        codex_auth("account-new", "refresh-new", &jwt(now_secs() + 86_400)),
    )
    .unwrap();
    let curl = fake_curl(root.path());

    let refresh = run_with_curl(
        root.path(),
        &curl,
        &["refresh", "race"],
        &[
            ("FAKE_AUTH_PATH", slot.join("auth.json").to_str().unwrap()),
            ("FAKE_REPLACE_FROM", replacement.to_str().unwrap()),
        ],
    );
    assert!(refresh.status.success(), "{}", combined(&refresh));
    assert!(!slot.join(STATUS_FILE).exists());
    let row = json_row(root.path(), "race");
    assert_eq!(row["warning"], serde_json::Value::Null, "{row}");
}

#[test]
fn successful_old_request_cannot_overwrite_credentials_replaced_in_flight() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "success-race",
        "account-old",
        "refresh-old",
        now_secs() - 60,
    );
    let replacement_bytes = codex_auth("account-new", "refresh-new", &jwt(now_secs() + 86_400));
    let replacement = root.path().join("replacement-auth.json");
    std::fs::write(&replacement, &replacement_bytes).unwrap();
    let response = serde_json::json!({
        "access_token": jwt(now_secs() + 172_800),
        "refresh_token": "response-for-old-account"
    })
    .to_string();
    let curl = fake_curl(root.path());

    let refresh = run_with_curl(
        root.path(),
        &curl,
        &["refresh", "success-race"],
        &[
            ("FAKE_STATUS", "200"),
            ("FAKE_BODY", response.as_str()),
            ("FAKE_AUTH_PATH", slot.join("auth.json").to_str().unwrap()),
            ("FAKE_REPLACE_FROM", replacement.to_str().unwrap()),
        ],
    );
    assert!(refresh.status.success(), "{}", combined(&refresh));
    assert_eq!(
        std::fs::read(slot.join("auth.json")).unwrap(),
        replacement_bytes,
        "a response for the old login overwrote its replacement"
    );
    assert!(!slot.join(STATUS_FILE).exists());
}
