//! Refresh coordination across callers and swapdex processes.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use swapdex::paths::Paths;
use swapdex::refresh::{refresh_slot, RefreshError};

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

    let process_path = root.path().join("claude");
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
