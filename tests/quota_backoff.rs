//! A throttled usage endpoint must not receive another request for the same
//! credential when quota is read again, including from another process.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct ReapedChild(Option<Child>);

impl ReapedChild {
    fn output(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Each command owns a process group, including fake curl and its
            // short-lived sleep child. Kill that exact group if an assertion
            // panics before the fixture opens the gate.
            let group = child.id() as libc::pid_t;
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    credential: PathBuf,
    curl: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join(".local/share/swapdex");
        let slot = store.join("slots/current");
        std::fs::create_dir_all(&slot).unwrap();

        std::fs::write(
            slot.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {
                    "accountUuid": "fixture-user",
                    "organizationUuid": "fixture-org",
                    "emailAddress": "fixture@example.com"
                }
            })
            .to_string(),
        )
        .unwrap();
        let credential = slot.join(".credentials.json");
        std::fs::write(
            &credential,
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "FIXTURE-ACCESS",
                    "refreshToken": "FIXTURE-REFRESH",
                    "expiresAt": 9_000_000_000_000_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            store.join("slots.json"),
            serde_json::json!([{
                "name": "fixture", "id": "current", "tool": "claude-code",
                "adopted": false, "config_dir": slot
            }])
            .to_string(),
        )
        .unwrap();
        std::fs::write(store.join("active-claude"), slot.to_str().unwrap()).unwrap();

        let curl = root.path().join("fake-curl");
        std::fs::write(
            &curl,
            r#"#!/bin/sh
cfg=$(cat)
case "$cfg" in
  *'https://api.anthropic.com/api/oauth/usage'*'Authorization: Bearer FIXTURE-ACCESS'*) ;;
  *'https://api.anthropic.com/api/oauth/usage'*'Authorization: Bearer SECOND-ACCESS'*) ;;
  *) exit 91 ;;
esac
printf x >> "$SWAPDEX_ROOT/usage-calls"
if [ -f "$SWAPDEX_ROOT/transport-fail" ]; then exit 7; fi
header_file=$(printf '%s\n' "$cfg" | sed -n 's/^dump-header = "\(.*\)"$/\1/p')
if [ -n "$header_file" ]; then
  if [ -f "$SWAPDEX_ROOT/response-headers" ]; then
    cat "$SWAPDEX_ROOT/response-headers" > "$header_file"
  else
    printf 'HTTP/2 429\r\n\r\n' > "$header_file"
  fi
fi
if [ -n "${FAKE_CURL_ENTERED:-}" ]; then
  printf x >> "$FAKE_CURL_ENTERED"
  gate_waits=0
  while [ ! -f "$FAKE_CURL_RELEASE" ]; do
    gate_waits=$((gate_waits + 1))
    [ "$gate_waits" -lt 500 ] || exit 92
    sleep 0.02
  done
fi
if [ -f "$SWAPDEX_ROOT/response" ]; then
  cat "$SWAPDEX_ROOT/response"
  exit 0
fi
printf '{"type":"error","error":{"type":"rate_limit_error"}}\n429'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();

        Self {
            root,
            credential,
            curl,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_swapdex"));
        command
            .args(["quota", "--json"])
            .env("SWAPDEX_ROOT", self.root.path())
            .env("HOME", self.root.path())
            .env("SWAPDEX_CURL", &self.curl)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
        command.process_group(0);
        command
    }

    fn calls(&self) -> usize {
        std::fs::read(self.root.path().join("usage-calls"))
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    }

    fn state_path(&self) -> PathBuf {
        let dir = self.root.path().join(".local/share/swapdex/usage-backoff");
        let states: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert_eq!(states.len(), 1, "expected one credential's state");
        states.into_iter().next().unwrap()
    }

    fn set_response(&self, body: &str, status: u32) {
        std::fs::write(
            self.root.path().join("response"),
            format!("{body}\n{status}"),
        )
        .unwrap();
    }

    fn expire_state(&self) -> PathBuf {
        let path = self.state_path();
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        state["throttled_at"] = (now - 61).into();
        state["retry_at"] = (now - 1).into();
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        path
    }

    fn row(&self, output: Output) -> serde_json::Value {
        assert!(
            output.status.success(),
            "quota exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let row = value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "fixture")
            .expect("fixture account in quota output");
        assert_eq!(row["email"], "fixture@example.com", "{row}");
        row.clone()
    }

    fn assert_throttled(&self, output: Output) {
        let row = self.row(output);
        assert_eq!(row["status"], "throttled", "{row}");
    }
}

fn wait_for(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sequential_quota_processes_share_one_throttled_usage_request() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.credential).unwrap();

    let first = fixture.command().output().unwrap();
    let second = fixture.command().output().unwrap();

    fixture.assert_throttled(first);
    fixture.assert_throttled(second);
    assert_eq!(std::fs::read(&fixture.credential).unwrap(), before);
    assert_eq!(fixture.calls(), 1, "repeated quota reads retried a 429");
}

#[test]
fn overlapping_quota_processes_share_one_throttled_usage_request() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.credential).unwrap();
    let entered = fixture.root.path().join("curl-entered");
    let release = fixture.root.path().join("curl-release");

    let mut first_command = fixture.command();
    first_command
        .env("FAKE_CURL_ENTERED", &entered)
        .env("FAKE_CURL_RELEASE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let first = ReapedChild(Some(first_command.spawn().unwrap()));
    wait_for(&entered);

    let mut second_command = fixture.command();
    second_command
        .env("FAKE_CURL_ENTERED", &entered)
        .env("FAKE_CURL_RELEASE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let second = ReapedChild(Some(second_command.spawn().unwrap()));
    std::thread::sleep(Duration::from_millis(250));
    std::fs::write(&release, "go").unwrap();

    fixture.assert_throttled(first.output());
    fixture.assert_throttled(second.output());
    assert_eq!(std::fs::read(&fixture.credential).unwrap(), before);
    assert_eq!(
        fixture.calls(),
        1,
        "overlapping quota reads sent multiple 429s"
    );
}

#[test]
fn final_retry_after_header_sets_a_larger_deadline() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.root.path().join("response-headers"),
        "HTTP/1.1 100 Continue\r\nRetry-After: 9000\r\n\r\n\
         HTTP/2 429\r\nrEtRy-AfTeR: 120\r\n\r\n",
    )
    .unwrap();

    fixture.assert_throttled(fixture.command().output().unwrap());

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.state_path()).unwrap()).unwrap();
    assert_eq!(
        state["retry_at"].as_i64().unwrap() - state["throttled_at"].as_i64().unwrap(),
        120
    );
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn zero_retry_after_uses_a_local_fallback() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.root.path().join("response-headers"),
        "HTTP/2 429\r\nRetry-After: 0\r\n\r\n",
    )
    .unwrap();

    fixture.assert_throttled(fixture.command().output().unwrap());

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.state_path()).unwrap()).unwrap();
    assert_eq!(
        state["retry_at"].as_i64().unwrap() - state["throttled_at"].as_i64().unwrap(),
        60
    );
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn invalid_state_reports_unavailable_without_another_request() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.credential).unwrap();
    fixture.assert_throttled(fixture.command().output().unwrap());
    std::fs::write(fixture.state_path(), b"{not-json").unwrap();

    let output = fixture.command().output().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let row = fixture.row(output);
    assert_eq!(row["status"], "unavailable", "{row}");
    assert!(row["detail"].as_str().unwrap().contains("invalid"), "{row}");
    assert!(value["offline"].is_null(), "{value}");
    assert_eq!(std::fs::read(&fixture.credential).unwrap(), before);
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn failed_and_deferred_reads_keep_cached_figures_and_age_until_recovery() {
    let fixture = Fixture::new();
    let cache_path = fixture
        .root
        .path()
        .join(".local/share/swapdex/quota-cache.json");
    let old_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 600;
    std::fs::write(
        &cache_path,
        serde_json::json!({
            "fixture": {"five_h": 42.0, "at": old_at}
        })
        .to_string(),
    )
    .unwrap();
    let old_cache = std::fs::read(&cache_path).unwrap();

    fixture.assert_throttled(fixture.command().output().unwrap());
    fixture.assert_throttled(fixture.command().output().unwrap());
    assert_eq!(std::fs::read(&cache_path).unwrap(), old_cache);
    assert_eq!(fixture.calls(), 1);

    let state_path = fixture.expire_state();
    fixture.set_response(r#"{"five_hour":{"utilization":33.0}}"#, 200);

    let row = fixture.row(fixture.command().output().unwrap());
    assert_eq!(row["status"], "ok", "{row}");
    assert_eq!(row["five_hour"]["used_pct"], 33.0);
    let cache: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(cache["fixture"]["five_h"], 33.0);
    assert!(cache["fixture"]["at"].as_i64().unwrap() > old_at);
    assert_eq!(fixture.calls(), 2);
    assert!(
        !state_path.exists(),
        "successful HTTP did not clear throttle history"
    );
}

#[test]
fn a_new_access_credential_is_independent_of_the_previous_cooldown() {
    let fixture = Fixture::new();
    fixture.assert_throttled(fixture.command().output().unwrap());
    let new_credential = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "SECOND-ACCESS", "refreshToken": "SECOND-REFRESH",
            "expiresAt": 9_000_000_000_000_i64
        }
    })
    .to_string();
    std::fs::write(&fixture.credential, new_credential.as_bytes()).unwrap();

    fixture.assert_throttled(fixture.command().output().unwrap());

    assert_eq!(fixture.calls(), 2);
    assert_eq!(
        std::fs::read(&fixture.credential).unwrap(),
        new_credential.as_bytes()
    );
}

#[test]
fn any_received_non_429_http_response_resets_throttle_history() {
    for (status, body, expected) in [
        (401, r#"{"error":"unauthorized"}"#, "expired"),
        (403, r#"{"error":"forbidden"}"#, "expired"),
        (200, "{}", "unexpected"),
        (500, r#"{"error":"server"}"#, "unexpected"),
    ] {
        let fixture = Fixture::new();
        fixture.assert_throttled(fixture.command().output().unwrap());
        let state_path = fixture.expire_state();
        fixture.set_response(body, status);

        let row = fixture.row(fixture.command().output().unwrap());
        assert_eq!(row["status"], expected, "HTTP {status}: {row}");
        assert!(!state_path.exists(), "HTTP {status} kept throttle history");
        assert_eq!(fixture.calls(), 2);

        fixture.set_response(
            r#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
            429,
        );
        fixture.assert_throttled(fixture.command().output().unwrap());
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
        assert_eq!(
            state["failures"], 1,
            "HTTP {status} did not reset the count"
        );
        assert_eq!(fixture.calls(), 3);
    }
}

#[test]
fn transport_failure_keeps_history_without_extending_the_deadline() {
    let fixture = Fixture::new();
    let credentials = std::fs::read(&fixture.credential).unwrap();
    fixture.assert_throttled(fixture.command().output().unwrap());
    let state_path = fixture.expire_state();
    let before = std::fs::read(&state_path).unwrap();
    let fail_marker = fixture.root.path().join("transport-fail");
    std::fs::write(&fail_marker, b"1").unwrap();

    let row = fixture.row(fixture.command().output().unwrap());
    assert_eq!(row["status"], "offline", "{row}");
    assert_eq!(std::fs::read(&state_path).unwrap(), before);
    assert_eq!(fixture.calls(), 2);
    std::fs::remove_file(fail_marker).unwrap();

    fixture.set_response("no HTTP response", 0);
    let row = fixture.row(fixture.command().output().unwrap());
    assert_eq!(row["status"], "offline", "{row}");
    assert!(state_path.exists(), "code 0 cleared throttle history");
    assert_eq!(std::fs::read(&state_path).unwrap(), before);
    assert_eq!(fixture.calls(), 3);
    std::fs::remove_file(fixture.root.path().join("response")).unwrap();

    fixture.assert_throttled(fixture.command().output().unwrap());
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
    assert_eq!(state["failures"], 2);
    assert_eq!(fixture.calls(), 4);
    assert_eq!(std::fs::read(&fixture.credential).unwrap(), credentials);
}
