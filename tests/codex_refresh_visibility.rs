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
if [ -n "$FAKE_COUNT_PATH" ]; then
    printf x >> "$FAKE_COUNT_PATH"
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

#[test]
fn curl_exiting_before_reading_config_does_not_kill_refresh() {
    let root = tempfile::tempdir().unwrap();
    // Exceed the pipe/socket buffer so an immediate-exit curl deterministically
    // closes stdin before the complete config has been written.
    let token = "synthetic-refresh-".repeat(65_536);
    let slot = seed_codex(root.path(), "early-exit", "synthetic-account", &token, 1);
    let before = std::fs::read(slot.join("auth.json")).unwrap();
    let curl = root.path().join("early-exit-curl");
    std::fs::write(&curl, "#!/bin/sh\nexit 22\n").unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = run_with_curl(root.path(), &curl, &["refresh", "early-exit"], &[]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "transport failure must return exit 4, not a signal: {}\n{}",
        output.status,
        combined(&output)
    );
    assert!(!combined(&output).contains("synthetic-refresh-"));
    assert_eq!(std::fs::read(slot.join("auth.json")).unwrap(), before);
    assert!(
        !slot.join(STATUS_FILE).exists(),
        "transport failure is not revocation"
    );
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

#[cfg(target_os = "linux")]
struct ReapedChild(std::process::Child);

#[cfg(target_os = "linux")]
impl Drop for ReapedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
impl ReapedChild {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
fn running_codex(root: &Path, config_dir: &Path) -> ReapedChild {
    let bin = root.join("fake-codex/codex");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    let sleep = if Path::new("/bin/sleep").exists() {
        "/bin/sleep"
    } else {
        "/usr/bin/sleep"
    };
    if !bin.exists() {
        std::os::unix::fs::symlink(sleep, &bin).unwrap();
    }
    let mut child = Command::new(&bin)
        .arg("30")
        .env_clear()
        .env("HOME", root)
        .env("CODEX_HOME", config_dir)
        .spawn()
        .unwrap();
    let environ = format!("/proc/{}/environ", child.id());
    for _ in 0..300 {
        if std::fs::read(&environ).is_ok_and(|bytes| {
            String::from_utf8_lossy(&bytes)
                .contains(&format!("CODEX_HOME={}", config_dir.to_string_lossy()))
        }) {
            return ReapedChild(child);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the fake Codex process never exposed its isolated environment");
}

#[cfg(target_os = "linux")]
#[test]
fn verified_native_codex_ownership_replaces_deferral_without_hiding_expiry_or_rejection() {
    use base64::Engine;
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "native",
        "workspace",
        "refresh-native",
        now_secs() + 3600,
    );
    let path = slot.join("auth.json");
    let mut auth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let subject =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"stable-user"}"#);
    auth["tokens"]["id_token"] = format!("header.{subject}.signature").into();
    let before = serde_json::to_vec(&auth).unwrap();
    std::fs::write(&path, &before).unwrap();
    let _native = running_codex(root.path(), &slot);
    let curl = fake_curl(root.path());
    let count = root.path().join("oauth-count");
    let sweep = run_with_curl(
        root.path(),
        &curl,
        &["refresh", "--keep-alive"],
        &[("FAKE_COUNT_PATH", count.to_str().unwrap())],
    );
    assert!(sweep.status.success(), "{}", combined(&sweep));
    assert!(
        combined(&sweep).contains("renewal is managed by Codex"),
        "{}",
        combined(&sweep)
    );
    assert!(
        !combined(&sweep).contains("renewal deferred"),
        "{}",
        combined(&sweep)
    );
    assert!(
        !count.exists(),
        "native ownership must not spend refresh tokens"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let row = json_row(root.path(), "native");
    assert_eq!(row["warning"], serde_json::Value::Null, "{row}");
    assert_eq!(row["renewal_owner"]["codex"], "native", "{row}");

    let fingerprint = swapdex::refresh_health::codex_credential_fingerprint(&slot).unwrap();
    swapdex::refresh_health::record_codex_rejection(&slot, &fingerprint, now_secs() * 1000)
        .unwrap();
    let row = json_row(root.path(), "native");
    assert!(
        row["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("refresh rejected"),
        "{row}"
    );

    swapdex::refresh_health::clear_codex_rejection(&slot, &fingerprint).unwrap();
    auth["tokens"]["access_token"] = jwt(1).into();
    std::fs::write(&path, serde_json::to_vec(&auth).unwrap()).unwrap();
    let row = json_row(root.path(), "native");
    assert!(
        row["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("expired"),
        "{row}"
    );
    assert!(
        row["renewal_owner"].get("codex").is_none(),
        "expired access must not read as ready: {row}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn due_codex_renewal_held_by_live_account_is_reported_as_deferred() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "due",
        "account-due",
        "refresh-due",
        now_secs() + 47 * 60 * 60,
    );
    let before = std::fs::read(slot.join("auth.json")).unwrap();
    let holder = root.path().join("running-codex");
    std::fs::create_dir_all(&holder).unwrap();
    std::fs::write(
        holder.join("auth.json"),
        codex_auth(
            "account-due",
            "holder-refresh",
            &jwt(now_secs() + 7 * 86_400),
        ),
    )
    .unwrap();
    let mut running = running_codex(root.path(), &holder);
    let curl = fake_curl(root.path());
    let count = root.path().join("curl-count");

    let keep_alive = run_with_curl(
        root.path(),
        &curl,
        &["refresh", "--keep-alive"],
        &[("FAKE_COUNT_PATH", count.to_str().unwrap())],
    );
    let said = combined(&keep_alive);
    assert!(keep_alive.status.success(), "{said}");
    assert!(said.contains("codex renewal deferred"), "{said}");
    assert!(said.contains("refresh unverified"), "{said}");
    assert!(
        !said.contains("every account has time left"),
        "a skipped renewal was reported as current: {said}"
    );
    assert_eq!(
        std::fs::read(&count).unwrap_or_default().len(),
        0,
        "the guarded renewal reached curl"
    );
    assert_eq!(
        std::fs::read(slot.join("auth.json")).unwrap(),
        before,
        "the guarded renewal changed auth.json"
    );

    let row = json_row(root.path(), "due");
    let warning = row["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("codex renewal deferred"), "{row}");
    assert!(warning.contains("refresh unverified"), "{row}");

    running.stop();
    let row = json_row(root.path(), "due");
    assert_eq!(row["warning"], serde_json::Value::Null, "{row}");

    let mut running = running_codex(root.path(), &holder);
    std::fs::write(
        slot.join("auth.json"),
        codex_auth(
            "account-due",
            "replacement-refresh",
            &jwt(now_secs() + 7 * 86_400),
        ),
    )
    .unwrap();
    let row = json_row(root.path(), "due");
    assert_eq!(row["warning"], serde_json::Value::Null, "{row}");
    running.stop();
}

#[cfg(target_os = "linux")]
#[test]
fn duplicate_due_codex_slots_held_by_one_live_account_are_all_deferred() {
    let root = tempfile::tempdir().unwrap();
    let slots = seed_slots(
        root.path(),
        &[
            ("due-a", "codex-slot-a", "codex"),
            ("due-b", "codex-slot-b", "codex"),
        ],
    );
    for (index, slot) in slots.iter().enumerate() {
        std::fs::write(
            slot.join("auth.json"),
            codex_auth(
                "account-shared",
                &format!("refresh-{index}"),
                &jwt(now_secs() + 47 * 60 * 60),
            ),
        )
        .unwrap();
    }
    let before: Vec<Vec<u8>> = slots
        .iter()
        .map(|slot| std::fs::read(slot.join("auth.json")).unwrap())
        .collect();
    let holder = root.path().join("running-codex");
    std::fs::create_dir_all(&holder).unwrap();
    std::fs::write(
        holder.join("auth.json"),
        codex_auth(
            "account-shared",
            "holder-refresh",
            &jwt(now_secs() + 7 * 86_400),
        ),
    )
    .unwrap();
    let _running = running_codex(root.path(), &holder);
    let curl = fake_curl(root.path());
    let count = root.path().join("curl-count");

    let keep_alive = run_with_curl(
        root.path(),
        &curl,
        &["refresh", "--keep-alive"],
        &[("FAKE_COUNT_PATH", count.to_str().unwrap())],
    );
    let said = combined(&keep_alive);
    assert_eq!(keep_alive.status.code(), Some(0), "{said}");
    for name in ["due-a", "due-b"] {
        assert!(
            said.contains(&format!("'{name}' codex renewal deferred")),
            "{said}"
        );
    }
    assert_eq!(said.matches("codex renewal deferred").count(), 2, "{said}");
    assert!(!said.contains("already being renewed"), "{said}");
    assert_eq!(
        std::fs::read(&count).unwrap_or_default().len(),
        0,
        "a held credential reached curl"
    );
    for (slot, expected) in slots.iter().zip(before) {
        assert_eq!(
            std::fs::read(slot.join("auth.json")).unwrap(),
            expected,
            "the guarded credential changed: {}",
            slot.display()
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn definitive_codex_health_verdicts_take_precedence_over_deferred_renewal() {
    let rejected_root = tempfile::tempdir().unwrap();
    let rejected_slot = seed_codex(
        rejected_root.path(),
        "rejected",
        "account-rejected",
        "refresh-rejected",
        now_secs() - 60,
    );
    let curl = fake_curl(rejected_root.path());
    let refresh = run_with_curl(rejected_root.path(), &curl, &["refresh", "rejected"], &[]);
    assert_eq!(refresh.status.code(), Some(4), "{}", combined(&refresh));
    std::fs::write(
        rejected_slot.join("auth.json"),
        codex_auth(
            "account-rejected",
            "refresh-rejected",
            &jwt(now_secs() + 47 * 60 * 60),
        ),
    )
    .unwrap();
    let rejected_holder = rejected_root.path().join("running-codex");
    std::fs::create_dir_all(&rejected_holder).unwrap();
    std::fs::write(
        rejected_holder.join("auth.json"),
        codex_auth(
            "account-rejected",
            "holder-refresh",
            &jwt(now_secs() + 7 * 86_400),
        ),
    )
    .unwrap();
    let _running = running_codex(rejected_root.path(), &rejected_holder);
    let row = json_row(rejected_root.path(), "rejected");
    let warning = row["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("codex refresh rejected"), "{row}");
    assert!(!warning.contains("renewal deferred"), "{row}");

    let expired_root = tempfile::tempdir().unwrap();
    let expired_slot = seed_codex(
        expired_root.path(),
        "expired",
        "account-expired",
        "refresh-expired",
        now_secs() - 60,
    );
    let expired_holder = expired_root.path().join("running-codex");
    std::fs::create_dir_all(&expired_holder).unwrap();
    std::fs::write(
        expired_holder.join("auth.json"),
        codex_auth(
            "account-expired",
            "holder-refresh",
            &jwt(now_secs() + 7 * 86_400),
        ),
    )
    .unwrap();
    let _running = running_codex(expired_root.path(), &expired_holder);
    let row = json_row(expired_root.path(), "expired");
    let warning = row["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("codex expired"), "{row}");
    assert!(!warning.contains("renewal deferred"), "{row}");
    assert!(expired_slot.join("auth.json").is_file());
}

#[cfg(target_os = "linux")]
#[test]
fn codex_deferred_renewal_does_not_warn_for_an_unrelated_provider() {
    let root = tempfile::tempdir().unwrap();
    let dirs = seed_slots(
        root.path(),
        &[
            ("due", "codex-slot", "codex"),
            ("claude-only", "claude-slot", "claude-code"),
        ],
    );
    std::fs::write(
        dirs[0].join("auth.json"),
        codex_auth(
            "shared-account-text",
            "codex-refresh",
            &jwt(now_secs() + 47 * 60 * 60),
        ),
    )
    .unwrap();
    std::fs::write(
        dirs[1].join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"claude-access","refreshToken":"claude-refresh","expiresAt":32503680000000}}"#,
    )
    .unwrap();
    std::fs::write(
        dirs[1].join(".claude.json"),
        br#"{"oauthAccount":{"accountUuid":"shared-account-text"}}"#,
    )
    .unwrap();
    let holder = root.path().join("running-codex");
    std::fs::create_dir_all(&holder).unwrap();
    std::fs::write(
        holder.join("auth.json"),
        codex_auth(
            "shared-account-text",
            "holder-refresh",
            &jwt(now_secs() + 7 * 86_400),
        ),
    )
    .unwrap();
    let _running = running_codex(root.path(), &holder);

    let codex = json_row(root.path(), "due");
    assert!(
        codex["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("codex renewal deferred")),
        "{codex}"
    );
    let claude = json_row(root.path(), "claude-only");
    assert_eq!(claude["warning"], serde_json::Value::Null, "{claude}");
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
    assert_eq!(refresh.status.code(), Some(4), "{refresh_text}");
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
        assert_eq!(refresh.status.code(), Some(4), "{}", combined(&refresh));
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
    assert_eq!(rejected.status.code(), Some(4), "{}", combined(&rejected));
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
        assert_eq!(refresh.status.code(), Some(4), "{}", combined(&refresh));
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
    assert_eq!(refresh.status.code(), Some(4), "{}", combined(&refresh));

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
    assert_eq!(second.status.code(), Some(4), "{}", combined(&second));
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
    assert_eq!(refresh.status.code(), Some(4), "{}", combined(&refresh));
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
    assert_eq!(refresh.status.code(), Some(4), "{}", combined(&refresh));
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
    assert_eq!(refresh.status.code(), Some(4), "{}", combined(&refresh));
    assert_eq!(
        std::fs::read(slot.join("auth.json")).unwrap(),
        replacement_bytes,
        "a response for the old login overwrote its replacement"
    );
    assert!(!slot.join(STATUS_FILE).exists());
}

#[test]
fn manual_refresh_reports_partial_failure_across_both_tools() {
    for claude_fails in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let dirs = seed_slots(
            root.path(),
            &[
                ("shared", "claude-slot", "claude-code"),
                ("shared", "codex-slot", "codex"),
            ],
        );
        let claude = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "synthetic-claude-access",
                "refreshToken": "synthetic-claude-refresh",
                "expiresAt": 1,
                "refreshTokenExpiresAt": if claude_fails { 1 } else { 32503680000000_i64 }
            }
        }))
        .unwrap();
        std::fs::write(dirs[0].join(".credentials.json"), &claude).unwrap();
        let codex = codex_auth(
            "codex-account",
            if claude_fails { "refresh-codex" } else { "" },
            &jwt(1),
        );
        std::fs::write(dirs[1].join("auth.json"), &codex).unwrap();
        let curl = fake_curl(root.path());
        let count = root.path().join("exchange-count");
        let answer = serde_json::json!({
            "access_token": jwt(now_secs() + 86_400),
            "refresh_token": "synthetic-renewed-refresh",
            "expires_in": 3600
        })
        .to_string();
        let output = run_with_curl(
            root.path(),
            &curl,
            &["refresh", "shared"],
            &[
                ("FAKE_STATUS", "200"),
                ("FAKE_BODY", &answer),
                ("FAKE_COUNT_PATH", count.to_str().unwrap()),
            ],
        );
        let said = combined(&output);
        assert_eq!(
            output.status.code(),
            Some(4),
            "partial renewal was reported as success: {said}"
        );
        assert!(
            said.contains("1 account(s) renewed"),
            "remaining account was not attempted: {said}"
        );
        assert_eq!(std::fs::read(count).unwrap().len(), 1);
        let (failed_dir, file, before) = if claude_fails {
            (&dirs[0], ".credentials.json", claude)
        } else {
            (&dirs[1], "auth.json", codex)
        };
        assert_eq!(std::fs::read(failed_dir.join(file)).unwrap(), before);
    }
}

#[test]
fn manual_refresh_keeps_provider_identities_separate() {
    for codex_account in ["shared-account", "distinct-codex-account"] {
        let root = tempfile::tempdir().unwrap();
        let dirs = seed_slots(
            root.path(),
            &[
                ("shared", "claude-slot", "claude-code"),
                ("shared", "codex-slot", "codex"),
                ("second", "codex-second-slot", "codex"),
            ],
        );
        let claude_identity = serde_json::json!({
            "oauthAccount": {"accountUuid": "shared-account"}
        });
        std::fs::write(dirs[0].join(".claude.json"), claude_identity.to_string()).unwrap();
        // A Codex directory can contain unrelated Claude metadata. Only its
        // Codex identity is relevant to the Codex renewal loop.
        std::fs::write(dirs[1].join(".claude.json"), claude_identity.to_string()).unwrap();
        std::fs::write(dirs[2].join(".claude.json"), claude_identity.to_string()).unwrap();
        std::fs::write(
            dirs[0].join(".credentials.json"),
            serde_json::json!({"claudeAiOauth": {
                "accessToken": "old-claude-access",
                "refreshToken": "old-claude-refresh",
                "expiresAt": 1
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dirs[1].join("auth.json"),
            codex_auth(codex_account, "old-codex-refresh", &jwt(1)),
        )
        .unwrap();
        std::fs::write(
            dirs[2].join("auth.json"),
            codex_auth("second-codex-account", "second-codex-refresh", &jwt(1)),
        )
        .unwrap();
        let curl = fake_curl(root.path());
        let count = root.path().join("exchange-count");
        let answer = serde_json::json!({
            "access_token": jwt(now_secs() + 86_400),
            "refresh_token": "new-synthetic-refresh",
            "expires_in": 3600
        })
        .to_string();
        let output = run_with_curl(
            root.path(),
            &curl,
            &["refresh"],
            &[
                ("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
                ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
                ("FAKE_STATUS", "200"),
                ("FAKE_BODY", &answer),
                ("FAKE_COUNT_PATH", count.to_str().unwrap()),
            ],
        );
        let said = combined(&output);
        assert_eq!(output.status.code(), Some(0), "{said}");
        assert_eq!(std::fs::read(count).unwrap().len(), 3, "{said}");
        assert!(said.contains("3 account(s) renewed"), "{said}");
        let codex: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dirs[1].join("auth.json")).unwrap()).unwrap();
        assert_eq!(codex["tokens"]["refresh_token"], "new-synthetic-refresh");
    }
}

#[test]
fn current_and_empty_manual_refresh_remain_successful() {
    let root = tempfile::tempdir().unwrap();
    let empty = run(root.path(), &["refresh"]);
    assert_eq!(empty.status.code(), Some(0), "{}", combined(&empty));
    let slot = seed_codex(
        root.path(),
        "current",
        "current-account",
        "current-refresh",
        now_secs() + 86_400,
    );
    let before = std::fs::read(slot.join("auth.json")).unwrap();
    let curl = fake_curl(root.path());
    let count = root.path().join("exchange-count");
    let current = run_with_curl(
        root.path(),
        &curl,
        &["refresh", "current"],
        &[("FAKE_COUNT_PATH", count.to_str().unwrap())],
    );
    assert_eq!(current.status.code(), Some(0), "{}", combined(&current));
    assert!(combined(&current).contains("already current"));
    assert!(!count.exists());
    assert_eq!(std::fs::read(slot.join("auth.json")).unwrap(), before);
}

#[test]
fn manual_refresh_does_not_call_unsigned_slots_current() {
    for tool in ["claude-code", "codex"] {
        for malformed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let dirs = seed_slots(root.path(), &[("empty", "empty-slot", tool)]);
            let file = if tool == "codex" {
                "auth.json"
            } else {
                ".credentials.json"
            };
            if malformed {
                std::fs::write(dirs[0].join(file), "{malformed").unwrap();
            }
            let curl = fake_curl(root.path());
            let count = root.path().join("exchange-count");
            let output = run_with_curl(
                root.path(),
                &curl,
                &["refresh", "empty"],
                &[("FAKE_COUNT_PATH", count.to_str().unwrap())],
            );
            let said = combined(&output);
            assert_eq!(
                output.status.code(),
                Some(4),
                "{tool} missing/unreadable login: {said}"
            );
            assert!(!said.contains("already current"), "{said}");
            assert!(!count.exists(), "missing/unreadable login reached OAuth");
            if malformed {
                assert_eq!(
                    std::fs::read_to_string(dirs[0].join(file)).unwrap(),
                    "{malformed"
                );
            } else {
                assert!(!dirs[0].join(file).exists());
            }
        }
    }
}
