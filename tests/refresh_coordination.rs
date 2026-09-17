//! Refresh coordination across callers and swapdex processes.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use swapdex::paths::Paths;
use swapdex::refresh::{
    keep_alive_sweep, refresh_codex_slot, refresh_slot, RefreshError, RefreshOutcome,
};

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvGuard(Vec<&'static str>);

impl EnvGuard {
    fn set(values: &[(&'static str, &Path)]) -> Self {
        let mut names = Vec::new();
        for (name, value) in values {
            std::env::set_var(name, value);
            names.push(*name);
        }
        Self(names)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for name in &self.0 {
            std::env::remove_var(name);
        }
    }
}

struct ReapedChild(Child);

impl Drop for ReapedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn seed_claude(root: &Path, account: &str) -> PathBuf {
    let store = root.join(".local/share/swapdex");
    let slot = store.join("slots/account");
    std::fs::create_dir_all(&slot).unwrap();
    let now = now_ms();
    std::fs::write(
        slot.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"OLD-AT","refreshToken":"OLD-RT","expiresAt":{},"refreshTokenExpiresAt":{}}}}}"#,
            now - 60_000,
            now + 86_400_000
        ),
    )
    .unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        format!(
            r#"{{"oauthAccount":{{"accountUuid":"{account}","organizationUuid":"org-{account}"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec(&serde_json::json!([{
            "name": "work",
            "id": "account",
            "config_dir": slot,
            "adopted": false,
            "tool": "claude-code"
        }]))
        .unwrap(),
    )
    .unwrap();
    slot
}

fn jwt(claims: serde_json::Value) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}

fn seed_codex(
    root: &Path,
    slot_id: &str,
    subject: &str,
    workspace: &str,
    refresh_token: &str,
) -> PathBuf {
    let slot = root.join(".local/share/swapdex/slots").join(slot_id);
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join("auth.json"),
        codex_credential(subject, workspace, refresh_token),
    )
    .unwrap();
    slot
}

fn codex_credential(subject: &str, workspace: &str, refresh_token: &str) -> Vec<u8> {
    codex_credential_with_access(
        subject,
        workspace,
        refresh_token,
        &jwt(serde_json::json!({"exp": 1})),
    )
}

fn codex_credential_with_access(
    subject: &str,
    workspace: &str,
    refresh_token: &str,
    access_token: &str,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": jwt(serde_json::json!({"sub": subject})),
            "access_token": access_token,
            "refresh_token": refresh_token,
            "account_id": workspace
        }
    }))
    .unwrap()
}

fn seed_opaque_codex(root: &Path, slot_id: &str, workspace: &str, refresh_token: &str) -> PathBuf {
    let slot = seed_codex(
        root,
        slot_id,
        "placeholder-subject",
        workspace,
        refresh_token,
    );
    let path = slot.join("auth.json");
    let mut credential: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    credential["tokens"]["id_token"] = "opaque-id-token".into();
    std::fs::write(path, serde_json::to_vec(&credential).unwrap()).unwrap();
    slot
}

fn write_codex_slots(root: &Path, slots: &[(&str, &Path)]) {
    let records: Vec<serde_json::Value> = slots
        .iter()
        .map(|(name, slot)| {
            serde_json::json!({
                "name": name,
                "id": slot.file_name().unwrap().to_string_lossy(),
                "config_dir": slot,
                "adopted": false,
                "tool": "codex"
            })
        })
        .collect();
    let store = root.join(".local/share/swapdex");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec(&records).unwrap(),
    )
    .unwrap();
}

fn make_curl(root: &Path, blocking: bool, status: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = root.join(if blocking {
        "blocking-curl"
    } else {
        "fake-curl"
    });
    let wait = if blocking {
        r#"
: > "$SWAPDEX_TEST_REFRESH_STARTED"
while [ ! -e "$SWAPDEX_TEST_REFRESH_RELEASE" ]; do sleep 0.01; done
"#
    } else {
        ""
    };
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
cat >/dev/null
printf x >> "$SWAPDEX_TEST_REFRESH_COUNT"
{wait}printf '%s\n{status}' '{{"access_token":"NEW-AT","refresh_token":"NEW-RT","expires_in":3600}}'
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn make_curl_without_rotated_token(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = root.join("fake-curl-without-rotated-token");
    std::fs::write(
        &path,
        r#"#!/bin/sh
cat >/dev/null
printf x >> "$SWAPDEX_TEST_REFRESH_COUNT"
printf '%s\n200' '{"access_token":"NEW-AT","expires_in":3600}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn make_rotating_curl(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = root.join("rotating-curl");
    std::fs::write(
        &path,
        r#"#!/bin/sh
cat >/dev/null
printf x >> "$SWAPDEX_TEST_REFRESH_COUNT"
turn=$(wc -c < "$SWAPDEX_TEST_REFRESH_COUNT" | tr -d ' ')
printf '{"access_token":"NEW-AT-%s","refresh_token":"NEW-RT-%s","expires_in":3600}\n200' "$turn" "$turn"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn claude_file(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join(".credentials.json")).unwrap()).unwrap()
}

fn expire_claude_file(dir: &Path) {
    let mut value = claude_file(dir);
    value["claudeAiOauth"]["expiresAt"] = (now_ms() - 1).into();
    std::fs::write(
        dir.join(".credentials.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

fn fixture_env(root: &Path, curl: &Path) -> EnvGuard {
    EnvGuard::set(&[
        ("SWAPDEX_ROOT", root),
        ("SWAPDEX_CURL", curl),
        ("SWAPDEX_TEST_REFRESH_COUNT", &root.join("refresh-count")),
        (
            "SWAPDEX_TEST_REFRESH_STARTED",
            &root.join("refresh-started"),
        ),
        (
            "SWAPDEX_TEST_REFRESH_RELEASE",
            &root.join("refresh-release"),
        ),
    ])
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "fixture never reached {}", path.display());
}

fn calls(root: &Path) -> usize {
    std::fs::read(root.join("refresh-count"))
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

#[test]
fn distinct_codex_users_in_one_workspace_refresh_independently() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let first_slot = seed_codex(
        root.path(),
        "codex-user-one",
        "subject-one",
        "shared-workspace-distinct-subjects",
        "REFRESH-ONE",
    );
    let second_slot = seed_codex(
        root.path(),
        "codex-user-two",
        "subject-two",
        "shared-workspace-distinct-subjects",
        "REFRESH-TWO",
    );
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    let first = refresh_codex_slot(&paths, &first_slot, now);
    let second = refresh_codex_slot(&paths, &second_slot, now);

    assert_eq!(first, Ok(RefreshOutcome::Renewed));
    assert_eq!(
        second,
        Ok(RefreshOutcome::Renewed),
        "a different JWT subject was merged into the first workspace member"
    );
    assert_eq!(
        calls(root.path()),
        2,
        "each user's refresh token is independent"
    );
}

#[test]
fn mixed_codex_metadata_with_one_refresh_token_is_exchanged_once_sequentially() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let known = seed_codex(
        root.path(),
        "mixed-sequential-known",
        "mixed-sequential-subject",
        "mixed-sequential-workspace",
        "MIXED-SEQUENTIAL-RT",
    );
    let opaque = seed_opaque_codex(
        root.path(),
        "mixed-sequential-opaque",
        "mixed-sequential-workspace",
        "MIXED-SEQUENTIAL-RT",
    );
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    let first = refresh_codex_slot(&paths, &known, now);
    let second = refresh_codex_slot(&paths, &opaque, now);

    assert_eq!(first, Ok(RefreshOutcome::Renewed));
    assert!(
        matches!(second, Err(RefreshError::AlreadyRefreshing)),
        "the opaque copy spent the rotating token again: {second:?}"
    );
    assert_eq!(calls(root.path()), 1, "one refresh token was spent twice");
}

#[test]
fn successful_codex_refresh_defers_a_same_subject_replacement_with_the_same_token() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "same-subject-replacement",
        "same-subject",
        "same-subject-workspace",
        "SAME-ROTATING-RT",
    );
    let curl = make_curl_without_rotated_token(root.path());
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    assert_eq!(
        refresh_codex_slot(&paths, &slot, now),
        Ok(RefreshOutcome::Renewed)
    );
    let replacement_access = jwt(serde_json::json!({"exp": 1, "generation": "replacement"}));
    let replacement = codex_credential_with_access(
        "same-subject",
        "same-subject-workspace",
        "SAME-ROTATING-RT",
        &replacement_access,
    );
    std::fs::write(slot.join("auth.json"), &replacement).unwrap();

    let result = refresh_codex_slot(&paths, &slot, now);

    assert!(
        matches!(result, Err(RefreshError::AlreadyRefreshing)),
        "a changed access credential claimed the prior success: {result:?}"
    );
    assert_eq!(
        std::fs::read(slot.join("auth.json")).unwrap(),
        replacement,
        "the replacement credential was overwritten"
    );
    assert_eq!(calls(root.path()), 1, "the shared token was spent again");
}

#[test]
fn successful_codex_refresh_does_not_claim_a_restored_input_blob() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "restored-input",
        "restored-subject",
        "restored-workspace",
        "RESTORED-INPUT-RT",
    );
    let original = std::fs::read(slot.join("auth.json")).unwrap();
    let curl = make_curl_without_rotated_token(root.path());
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    assert_eq!(
        refresh_codex_slot(&paths, &slot, now),
        Ok(RefreshOutcome::Renewed)
    );
    std::fs::write(slot.join("auth.json"), &original).unwrap();

    let result = refresh_codex_slot(&paths, &slot, now);

    assert!(
        matches!(result, Err(RefreshError::AlreadyRefreshing)),
        "the restored input blob claimed a success persisted elsewhere: {result:?}"
    );
    assert_eq!(
        std::fs::read(slot.join("auth.json")).unwrap(),
        original,
        "the restored input blob was overwritten"
    );
    assert_eq!(calls(root.path()), 1, "the shared token was spent again");
}

#[test]
fn successful_codex_refresh_defers_a_changed_subject_in_a_later_process() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "changed-subject-process",
        "subject-before",
        "same-workspace",
        "SAME-PROCESS-RT",
    );
    write_codex_slots(root.path(), &[("work", &slot)]);
    let curl = make_curl_without_rotated_token(root.path());
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_swapdex"))
            .args(["refresh", "work"])
            .env("SWAPDEX_ROOT", root.path())
            .env("SWAPDEX_CURL", &curl)
            .env(
                "SWAPDEX_TEST_REFRESH_COUNT",
                root.path().join("refresh-count"),
            )
            .output()
            .unwrap()
    };

    let first = run();
    assert!(
        first.status.success(),
        "first refresh exited {}",
        first.status
    );
    let replacement_access = jwt(serde_json::json!({"exp": 1, "generation": "replacement"}));
    let replacement = codex_credential_with_access(
        "subject-after",
        "same-workspace",
        "SAME-PROCESS-RT",
        &replacement_access,
    );
    std::fs::write(slot.join("auth.json"), &replacement).unwrap();

    let second = run();

    assert_eq!(
        second.status.code(),
        Some(4),
        "a changed subject claimed the prior process's success: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    assert_eq!(
        std::fs::read(slot.join("auth.json")).unwrap(),
        replacement,
        "the later process overwrote the replacement credential"
    );
    assert_eq!(calls(root.path()), 1, "the shared token was spent again");
}

#[test]
fn a_replacement_with_a_different_refresh_token_remains_independent() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_codex(
        root.path(),
        "different-token-replacement",
        "different-token-subject",
        "different-token-workspace",
        "FIRST-ROTATING-RT",
    );
    let curl = make_curl_without_rotated_token(root.path());
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    assert_eq!(
        refresh_codex_slot(&paths, &slot, now),
        Ok(RefreshOutcome::Renewed)
    );
    std::fs::write(
        slot.join("auth.json"),
        codex_credential(
            "different-token-subject",
            "different-token-workspace",
            "SECOND-ROTATING-RT",
        ),
    )
    .unwrap();

    let result = refresh_codex_slot(&paths, &slot, now);

    assert_eq!(result, Ok(RefreshOutcome::Renewed));
    assert_eq!(
        calls(root.path()),
        2,
        "the new token was incorrectly blocked"
    );
}

#[test]
fn mixed_codex_metadata_with_one_refresh_token_is_serialized_across_processes() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let known = seed_codex(
        root.path(),
        "mixed-process-known",
        "mixed-process-subject",
        "mixed-process-workspace",
        "MIXED-PROCESS-RT",
    );
    let opaque = seed_opaque_codex(
        root.path(),
        "mixed-process-opaque",
        "mixed-process-workspace",
        "MIXED-PROCESS-RT",
    );
    write_codex_slots(root.path(), &[("known", &known), ("opaque", &opaque)]);
    let curl = make_curl(root.path(), true, 200);

    let spawn = |name: &str| {
        Command::new(env!("CARGO_BIN_EXE_swapdex"))
            .args(["refresh", name])
            .env("SWAPDEX_ROOT", root.path())
            .env("SWAPDEX_CURL", &curl)
            .env(
                "SWAPDEX_TEST_REFRESH_COUNT",
                root.path().join("refresh-count"),
            )
            .env(
                "SWAPDEX_TEST_REFRESH_STARTED",
                root.path().join("refresh-started"),
            )
            .env(
                "SWAPDEX_TEST_REFRESH_RELEASE",
                root.path().join("refresh-release"),
            )
            .spawn()
            .unwrap()
    };

    let mut first = ReapedChild(spawn("known"));
    wait_for(&root.path().join("refresh-started"));
    let mut second = ReapedChild(spawn("opaque"));
    std::thread::sleep(Duration::from_millis(200));
    let before_release = calls(root.path());
    std::fs::write(root.path().join("refresh-release"), b"go").unwrap();
    let first_status = first.0.wait().unwrap();
    let second_status = second.0.wait().unwrap();

    assert_eq!(
        before_release, 1,
        "both processes entered the OAuth exchange concurrently"
    );
    assert!(first_status.success(), "leader exited {first_status}");
    assert_eq!(
        second_status.code(),
        Some(4),
        "a different source must defer instead of claiming the leader's success"
    );
    assert_eq!(calls(root.path()), 1, "the refresh token was spent twice");
}

#[test]
fn opaque_codex_copy_defers_to_a_known_native_holder_of_the_same_token() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let native = seed_codex(
        root.path(),
        "mixed-native-known",
        "mixed-native-subject",
        "mixed-native-workspace",
        "MIXED-NATIVE-RT",
    );
    let opaque = seed_opaque_codex(
        root.path(),
        "mixed-native-opaque",
        "mixed-native-workspace",
        "MIXED-NATIVE-RT",
    );
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());

    let process_path = root.path().join("native-bin/codex");
    std::fs::create_dir_all(process_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", &process_path).unwrap();
    let mut held = ReapedChild(
        Command::new(&process_path)
            .arg("30")
            .env_clear()
            .env("HOME", root.path())
            .env("CODEX_HOME", &native)
            .spawn()
            .unwrap(),
    );
    wait_for(Path::new(&format!("/proc/{}/environ", held.0.id())));

    let result = refresh_codex_slot(&paths, &opaque, now_ms());
    held.0.kill().unwrap();
    held.0.wait().unwrap();

    assert!(
        matches!(result, Err(RefreshError::InUse)),
        "opaque metadata bypassed native ownership of the token: {result:?}"
    );
    assert_eq!(calls(root.path()), 0, "OAuth ran behind a native holder");
}

#[test]
fn concurrent_callers_wait_for_and_share_the_refresh_result() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "concurrent-callers");
    let curl = make_curl(root.path(), true, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    let first_paths = paths.clone();
    let first_slot = slot.clone();
    let first = std::thread::spawn(move || refresh_slot(&first_paths, &first_slot, now));
    wait_for(&root.path().join("refresh-started"));

    let second_paths = paths.clone();
    let second_slot = slot.clone();
    let (sent, received) = mpsc::channel();
    let second = std::thread::spawn(move || {
        let result = refresh_slot(&second_paths, &second_slot, now);
        sent.send(()).unwrap();
        result
    });
    let returned_while_exchange_running = received.recv_timeout(Duration::from_millis(150)).is_ok();
    std::fs::write(root.path().join("refresh-release"), b"go").unwrap();

    let first_result = first.join().unwrap();
    let second_result = second.join().unwrap();
    assert!(
        !returned_while_exchange_running,
        "the follower returned before the leader finished: {second_result:?}"
    );
    assert!(first_result.is_ok(), "leader failed: {first_result:?}");
    assert!(
        second_result.is_ok(),
        "follower did not receive the leader's success: {second_result:?}"
    );
    assert_eq!(calls(root.path()), 1, "the refresh token was spent twice");
}

#[test]
fn participating_swapdex_processes_serialize_one_exchange() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    seed_claude(root.path(), "cross-process");
    let curl = make_curl(root.path(), true, 200);

    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_swapdex"))
            .args(["refresh", "work"])
            .env("SWAPDEX_ROOT", root.path())
            .env("SWAPDEX_CURL", &curl)
            .env(
                "SWAPDEX_TEST_REFRESH_COUNT",
                root.path().join("refresh-count"),
            )
            .env(
                "SWAPDEX_TEST_REFRESH_STARTED",
                root.path().join("refresh-started"),
            )
            .env(
                "SWAPDEX_TEST_REFRESH_RELEASE",
                root.path().join("refresh-release"),
            )
            .spawn()
            .unwrap()
    };

    let mut first = ReapedChild(spawn());
    wait_for(&root.path().join("refresh-started"));
    let mut second = ReapedChild(spawn());
    std::thread::sleep(Duration::from_millis(200));
    let before_release = calls(root.path());
    std::fs::write(root.path().join("refresh-release"), b"go").unwrap();
    let first_status = first.0.wait().unwrap();
    let second_status = second.0.wait().unwrap();

    assert_eq!(
        before_release, 1,
        "both processes entered the OAuth exchange concurrently"
    );
    assert!(first_status.success(), "leader exited {first_status}");
    assert!(second_status.success(), "follower exited {second_status}");
    assert_eq!(calls(root.path()), 1, "the refresh token was spent twice");
}

#[test]
fn an_in_use_preflight_does_not_poison_the_next_attempt() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "held-then-free");
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());

    let process_path = root.path().join("inherited-child");
    std::os::unix::fs::symlink("/bin/sleep", &process_path).unwrap();
    let mut held = ReapedChild(
        Command::new(&process_path)
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", &slot)
            .spawn()
            .unwrap(),
    );
    wait_for(Path::new(&format!("/proc/{}/environ", held.0.id())));

    let now = now_ms();
    let while_held = refresh_slot(&paths, &slot, now);
    held.0.kill().unwrap();
    held.0.wait().unwrap();
    let after_exit = refresh_slot(&paths, &slot, now);

    assert!(matches!(while_held, Err(RefreshError::InUse)));
    assert!(
        after_exit.is_ok(),
        "an attempt that never exchanged poisoned the gate: {after_exit:?}"
    );
    assert_eq!(calls(root.path()), 1);
}

#[test]
fn one_native_claude_session_survives_two_same_store_refreshes() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "same-store-native");
    let curl = make_rotating_curl(root.path());
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());

    let process_path = root.path().join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &process_path).unwrap();
    let held = ReapedChild(
        Command::new(&process_path)
            .arg("30")
            .env("HOME", root.path())
            .env("CLAUDE_CONFIG_DIR", &slot)
            .env("SWAPDEX_TEST_NATIVE_REFRESH_LOCKS", "1")
            .spawn()
            .unwrap(),
    );
    wait_for(Path::new(&format!("/proc/{}/environ", held.0.id())));

    assert_eq!(
        refresh_slot(&paths, &slot, now_ms()),
        Ok(RefreshOutcome::Renewed)
    );
    assert_eq!(
        claude_file(&slot)["claudeAiOauth"]["refreshToken"],
        "NEW-RT-1"
    );
    expire_claude_file(&slot);
    assert_eq!(
        refresh_slot(&paths, &slot, now_ms()),
        Ok(RefreshOutcome::Renewed)
    );
    assert_eq!(
        claude_file(&slot)["claudeAiOauth"]["refreshToken"],
        "NEW-RT-2"
    );
    assert_eq!(calls(root.path()), 2);
    assert!(Path::new(&format!("/proc/{}", held.0.id())).exists());
}

#[test]
fn unproved_native_version_still_defers_same_store_refresh() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "unproved-native-version");
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());

    let process_path = root.path().join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &process_path).unwrap();
    let held = ReapedChild(
        Command::new(&process_path)
            .arg("30")
            .env("HOME", root.path())
            .env("CLAUDE_CONFIG_DIR", &slot)
            .spawn()
            .unwrap(),
    );
    wait_for(Path::new(&format!("/proc/{}/environ", held.0.id())));

    assert_eq!(
        refresh_slot(&paths, &slot, now_ms()),
        Err(RefreshError::InUse)
    );
    assert_eq!(calls(root.path()), 0);
}

#[test]
fn native_refresh_lock_blocks_oauth_until_it_is_released() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "native-lock-held");
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let native_lock = slot.join(".oauth_refresh.lock");
    std::fs::create_dir(&native_lock).unwrap();

    let worker_slot = slot.clone();
    let refresh = std::thread::spawn(move || refresh_slot(&paths, &worker_slot, now_ms()));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(calls(root.path()), 0, "OAuth bypassed Claude's native lock");
    std::fs::remove_dir(&native_lock).unwrap();
    assert_eq!(refresh.join().unwrap(), Ok(RefreshOutcome::Renewed));
    assert_eq!(calls(root.path()), 1);
}

#[test]
fn changed_credential_under_native_lock_is_never_exchanged() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "changed-under-lock");
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let native_lock = slot.join(".oauth_refresh.lock");
    std::fs::create_dir(&native_lock).unwrap();

    let worker_slot = slot.clone();
    let refresh = std::thread::spawn(move || refresh_slot(&paths, &worker_slot, now_ms()));
    std::thread::sleep(Duration::from_millis(150));
    let replacement = br#"{"claudeAiOauth":{"accessToken":"LOGIN-AT","refreshToken":"LOGIN-RT","expiresAt":9999999999999}}"#;
    std::fs::write(slot.join(".credentials.json"), replacement).unwrap();
    std::fs::remove_dir(&native_lock).unwrap();

    assert!(matches!(
        refresh.join().unwrap(),
        Err(RefreshError::AlreadyRefreshing)
    ));
    assert_eq!(calls(root.path()), 0);
    assert_eq!(
        std::fs::read(slot.join(".credentials.json")).unwrap(),
        replacement
    );
}

#[test]
fn default_native_authority_survives_holder_exit_without_using_stale_slot() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "default-source-bound");
    let native = root.path().join(".claude");
    std::fs::create_dir(&native).unwrap();
    std::fs::copy(
        slot.join(".credentials.json"),
        native.join(".credentials.json"),
    )
    .unwrap();
    std::fs::write(
        root.path().join(".claude.json"),
        std::fs::read(slot.join(".claude.json")).unwrap(),
    )
    .unwrap();
    let curl = make_rotating_curl(root.path());
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());

    let process_path = root.path().join("native-bin/claude");
    std::fs::create_dir_all(process_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", &process_path).unwrap();
    let mut held = ReapedChild(
        Command::new(&process_path)
            .arg("30")
            .env_clear()
            .env("HOME", root.path())
            .env("SWAPDEX_TEST_NATIVE_REFRESH_LOCKS", "1")
            .spawn()
            .unwrap(),
    );
    wait_for(Path::new(&format!("/proc/{}/environ", held.0.id())));

    let (renewed, failed) = keep_alive_sweep(&paths, &[("work".into(), slot.clone())], now_ms());
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(renewed, ["work"]);
    assert_eq!(
        claude_file(&native)["claudeAiOauth"]["refreshToken"],
        "NEW-RT-1"
    );
    assert_eq!(
        claude_file(&slot)["claudeAiOauth"]["refreshToken"],
        "OLD-RT"
    );

    held.0.kill().unwrap();
    held.0.wait().unwrap();
    expire_claude_file(&native);
    assert_eq!(
        refresh_slot(&paths, &slot, now_ms()),
        Ok(RefreshOutcome::Renewed)
    );
    assert_eq!(
        claude_file(&native)["claudeAiOauth"]["refreshToken"],
        "NEW-RT-2"
    );
    assert_eq!(
        claude_file(&slot)["claudeAiOauth"]["refreshToken"],
        "OLD-RT"
    );
    assert_eq!(calls(root.path()), 2);
}

#[test]
fn claude_success_does_not_overwrite_a_login_replaced_in_flight() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "replaced-in-flight");
    let curl = make_curl(root.path(), true, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    let worker_paths = paths.clone();
    let worker_slot = slot.clone();
    let refresh = std::thread::spawn(move || refresh_slot(&worker_paths, &worker_slot, now));
    wait_for(&root.path().join("refresh-started"));

    let replacement = br#"{"claudeAiOauth":{"accessToken":"LOGIN-AT","refreshToken":"LOGIN-RT","expiresAt":9999999999999}}"#;
    std::fs::write(slot.join(".credentials.json"), replacement).unwrap();
    std::fs::write(root.path().join("refresh-release"), b"go").unwrap();
    let result = refresh.join().unwrap();

    assert!(
        matches!(result, Err(RefreshError::AlreadyRefreshing)),
        "the stale response was not discarded: {result:?}"
    );
    assert_eq!(
        std::fs::read(slot.join(".credentials.json")).unwrap(),
        replacement,
        "a response for the old blob overwrote the newer login"
    );
}

#[test]
fn a_slot_without_identity_cannot_spend_a_native_holders_refresh_generation() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "missing-identity");
    let native = root.path().join(".claude");
    std::fs::create_dir_all(&native).unwrap();
    std::fs::copy(
        slot.join(".credentials.json"),
        native.join(".credentials.json"),
    )
    .unwrap();
    std::fs::copy(slot.join(".claude.json"), root.path().join(".claude.json")).unwrap();
    std::fs::remove_file(slot.join(".claude.json")).unwrap();
    let process = root.path().join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &process).unwrap();
    let _held = ReapedChild(
        Command::new(process)
            .arg("30")
            .env_clear()
            .env("HOME", root.path())
            .spawn()
            .unwrap(),
    );
    wait_for(Path::new(&format!("/proc/{}/environ", _held.0.id())));
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    assert_eq!(
        refresh_slot(&paths, &slot, now_ms()),
        Err(RefreshError::InUse)
    );
    assert_eq!(calls(root.path()), 0);
}

#[test]
fn an_expired_default_native_holder_still_blocks_a_twin_refresh() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "default-holder");
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());

    std::fs::write(
        root.path().join(".claude.json"),
        br#"{"oauthAccount":{"accountUuid":"default-holder","organizationUuid":"org-default-holder"}}"#,
    )
    .unwrap();
    let default_dir = root.path().join(".claude");
    std::fs::create_dir_all(&default_dir).unwrap();
    std::fs::write(
        default_dir.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"EXPIRED","refreshToken":"NATIVE-RT","expiresAt":1}}"#,
    )
    .unwrap();
    let process_path = root.path().join("native-bin/claude");
    std::fs::create_dir_all(process_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", &process_path).unwrap();
    let _held = ReapedChild(
        Command::new(&process_path)
            .arg("30")
            .env_clear()
            .env("HOME", root.path())
            .spawn()
            .unwrap(),
    );
    wait_for(Path::new(&format!("/proc/{}/environ", _held.0.id())));

    let result = refresh_slot(&paths, &slot, now_ms());
    assert!(
        matches!(result, Err(RefreshError::InUse)),
        "an unusable native holder did not retain the safety guard: {result:?}"
    );
    assert_eq!(calls(root.path()), 0, "OAuth ran behind the native holder");
}

#[test]
fn a_recent_success_is_not_reused_for_an_unrelated_replacement_generation() {
    let _serial = test_lock();
    let root = tempfile::tempdir().unwrap();
    let slot = seed_claude(root.path(), "replacement-generation");
    let curl = make_curl(root.path(), false, 200);
    let _env = fixture_env(root.path(), &curl);
    let paths = Paths::rooted(root.path());
    let now = now_ms();

    assert!(refresh_slot(&paths, &slot, now).is_ok());
    std::fs::write(
        slot.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"LOGIN-AT","refreshToken":"LOGIN-RT","expiresAt":{},"refreshTokenExpiresAt":{}}}}}"#,
            now - 1,
            now + 86_400_000
        ),
    )
    .unwrap();

    let replacement_result = refresh_slot(&paths, &slot, now);
    assert!(
        replacement_result.is_ok(),
        "the replacement generation was not renewed: {replacement_result:?}"
    );
    assert_eq!(
        calls(root.path()),
        2,
        "the prior success was incorrectly reused for a different generation"
    );
    let credential = std::fs::read_to_string(slot.join(".credentials.json")).unwrap();
    assert!(credential.contains("NEW-AT"), "replacement stayed stale");
}
