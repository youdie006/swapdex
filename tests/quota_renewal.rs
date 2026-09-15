//! Quota reads must use the credential produced by an eligible slot renewal.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct ReapedChild(Option<Child>);

impl ReapedChild {
    fn wait_with_output(&mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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

struct Fixture {
    root: tempfile::TempDir,
    slot: PathBuf,
    curl: PathBuf,
}

impl Fixture {
    fn new(with_saved_profile: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join(".local/share/swapdex");
        let slot = store.join("slots/current");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(
            slot.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {
                    "accountUuid": "current-user",
                    "organizationUuid": "current-org",
                    "emailAddress": "current@example.com"
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            slot.join(".credentials.json"),
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "OLD-ACCESS",
                    "refreshToken": "OLD-REFRESH",
                    "expiresAt": 1,
                    "refreshTokenExpiresAt": 9_000_000_000_000_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            store.join("slots.json"),
            serde_json::json!([{
                "name": "work",
                "id": "current",
                "tool": "claude-code",
                "adopted": false,
                "config_dir": slot
            }])
            .to_string(),
        )
        .unwrap();
        std::fs::write(store.join("active-claude"), slot.to_str().unwrap()).unwrap();

        if with_saved_profile {
            let saved = store.join("accounts/work/claude-code");
            std::fs::create_dir_all(&saved).unwrap();
            std::fs::write(
                saved.join("oauth_account"),
                serde_json::json!({
                    "accountUuid": "old-snapshot-user",
                    "emailAddress": "old-snapshot@example.com"
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                saved.join("credentials"),
                serde_json::json!({
                    "claudeAiOauth": {
                        "accessToken": "OLD-SNAPSHOT-ACCESS",
                        "expiresAt": 9_000_000_000_000_i64
                    }
                })
                .to_string(),
            )
            .unwrap();
        }

        let curl = root.path().join("fake-curl");
        std::fs::write(
            &curl,
            r#"#!/bin/sh
cfg=$(cat)
case "$cfg" in
  *oauth.test*)
    printf x >> "$HOME/oauth-calls"
    case "$cfg" in *OLD-REFRESH*) ;; *) exit 92 ;; esac
    if [ "${TEST_PAUSE_OAUTH:-}" = 1 ]; then
      : > "$HOME/oauth-started"
      n=0
      while [ ! -f "$HOME/oauth-release" ]; do
        n=$((n + 1))
        [ "$n" -lt 1000 ] || exit 93
        sleep 0.01
      done
    fi
    if [ "${TEST_OAUTH_RESULT:-ok}" = refused ]; then
      printf '{"error_description":"refresh refused"}\n401'
    else
      printf '{"access_token":"NEW-ACCESS","refresh_token":"NEW-REFRESH","expires_in":3600}\n200'
    fi
    ;;
  *OLD-ACCESS*)
    printf x >> "$HOME/old-usage-calls"
    printf '{"error":"old access rejected"}\n401'
    ;;
  *OLD-SNAPSHOT-ACCESS*)
    printf x >> "$HOME/snapshot-usage-calls"
    printf '{"five_hour":{"utilization":99}}\n200'
    ;;
  *NEW-ACCESS*)
    printf x >> "$HOME/new-usage-calls"
    printf '{"five_hour":{"utilization":12}}\n200'
    ;;
  *NATIVE-ACCESS*)
    printf x >> "$HOME/native-usage-calls"
    printf '{"five_hour":{"utilization":23}}\n200'
    ;;
  *REPLACEMENT-ACCESS*)
    printf x >> "$HOME/replacement-usage-calls"
    printf '{"five_hour":{"utilization":34}}\n200'
    ;;
  *) exit 91 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();

        Self { root, slot, curl }
    }

    fn quota_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_swapdex"));
        command
            .args(["quota", "--json"])
            .env("SWAPDEX_ROOT", self.root.path())
            .env("HOME", self.root.path())
            .env("SWAPDEX_CURL", &self.curl)
            .env("SWAPDEX_OAUTH_URL", "https://oauth.test/token")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .env_remove("SWAPDEX_TEST_REFRESH_COUNT")
            .env_remove("SWAPDEX_TEST_REFRESH_STARTED")
            .env_remove("SWAPDEX_TEST_REFRESH_RELEASE");
        command
    }

    fn quota(&self) -> serde_json::Value {
        let out = self.quota_command().output().unwrap();
        self.output(out)
    }

    fn output(&self, out: Output) -> serde_json::Value {
        assert!(
            out.status.success(),
            "quota exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "quota returned invalid JSON ({error}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    fn calls(&self, name: &str) -> usize {
        std::fs::read(self.root.path().join(name))
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    }

    fn row<'a>(&self, value: &'a serde_json::Value) -> &'a serde_json::Value {
        value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "work")
            .expect("work quota row")
    }

    fn assert_first_read_used_renewal(&self, value: &serde_json::Value) {
        let row = self.row(value);
        assert_eq!(row["status"], "ok", "{row}");
        assert_eq!(row["email"], "current@example.com", "{row}");
        assert_eq!(row["five_hour"]["used_pct"], 12.0, "{row}");
        assert_eq!(self.calls("oauth-calls"), 1, "one OAuth exchange");
        assert_eq!(self.calls("new-usage-calls"), 1, "new access token used");
        assert_eq!(self.calls("old-usage-calls"), 0, "old access token leaked");
        assert_eq!(
            self.calls("snapshot-usage-calls"),
            0,
            "saved profile substituted for its backing slot"
        );
        let credential: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.slot.join(".credentials.json")).unwrap())
                .unwrap();
        assert_eq!(credential["claudeAiOauth"]["accessToken"], "NEW-ACCESS");
        assert_eq!(credential["claudeAiOauth"]["refreshToken"], "NEW-REFRESH");
    }
}

#[test]
fn slot_only_quota_uses_the_renewed_credential_on_its_first_read() {
    let fixture = Fixture::new(false);

    let value = fixture.quota();

    fixture.assert_first_read_used_renewal(&value);
}

#[test]
fn failed_renewal_is_explicit_and_does_not_send_the_old_access_token() {
    let fixture = Fixture::new(false);

    let out = fixture
        .quota_command()
        .env("TEST_OAUTH_RESULT", "refused")
        .output()
        .unwrap();
    let value = fixture.output(out);
    let row = fixture.row(&value);

    assert_eq!(row["status"], "offline", "{row}");
    assert!(
        row["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("refresh refused")),
        "renewal failure was hidden: {row}"
    );
    assert_eq!(fixture.calls("oauth-calls"), 1, "one OAuth attempt");
    assert_eq!(fixture.calls("old-usage-calls"), 0, "old access token used");
    assert_eq!(fixture.calls("new-usage-calls"), 0, "failed answer used");
}

#[test]
#[cfg(target_os = "linux")]
fn in_use_renewal_is_deferred_without_oauth_or_an_old_quota_read() {
    let fixture = Fixture::new(false);
    let claude = fixture.root.path().join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &claude).unwrap();
    let mut held = ReapedChild(Some(
        Command::new(&claude)
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", &fixture.slot)
            .spawn()
            .unwrap(),
    ));
    wait_for(Path::new(&format!(
        "/proc/{}/environ",
        held.0.as_ref().unwrap().id()
    )));

    let value = fixture.quota();
    let row = fixture.row(&value);

    assert_eq!(row["status"], "offline", "{row}");
    assert!(
        row["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("renewal deferred")),
        "deferred ownership was hidden: {row}"
    );
    assert_eq!(fixture.calls("oauth-calls"), 0, "OAuth ran behind Claude");
    assert_eq!(fixture.calls("old-usage-calls"), 0, "old access token used");
    held.0.as_mut().unwrap().kill().unwrap();
    held.0.as_mut().unwrap().wait().unwrap();
    held.0 = None;
}

#[test]
#[cfg(target_os = "linux")]
fn a_current_native_owner_supplies_only_the_same_accounts_credential() {
    let fixture = Fixture::new(false);
    let native = fixture.root.path().join(".claude");
    std::fs::create_dir_all(&native).unwrap();
    std::fs::write(
        fixture.root.path().join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "current-user",
                "organizationUuid": "current-org",
                "emailAddress": "current@example.com"
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        native.join(".credentials.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "NATIVE-ACCESS",
                "refreshToken": "NATIVE-REFRESH",
                "expiresAt": 9_000_000_000_000_i64
            }
        })
        .to_string(),
    )
    .unwrap();
    let claude = fixture.root.path().join("native-bin/claude");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", &claude).unwrap();
    let mut held = ReapedChild(Some(
        Command::new(&claude)
            .arg("30")
            .env_clear()
            .env("HOME", fixture.root.path())
            .spawn()
            .unwrap(),
    ));
    let pid = held.0.as_ref().unwrap().id();
    wait_for(Path::new(&format!("/proc/{pid}/environ")));

    let value = fixture.quota();
    let row = fixture.row(&value);

    assert_eq!(row["status"], "ok", "{row}");
    assert_eq!(row["email"], "current@example.com", "{row}");
    assert_eq!(row["five_hour"]["used_pct"], 23.0, "{row}");
    assert_eq!(fixture.calls("native-usage-calls"), 1);
    assert_eq!(fixture.calls("oauth-calls"), 0, "native token was rotated");
    assert_eq!(
        fixture.calls("old-usage-calls"),
        0,
        "slot token substituted"
    );
    held.0.as_mut().unwrap().kill().unwrap();
    held.0.as_mut().unwrap().wait().unwrap();
    held.0 = None;
}

#[test]
fn a_login_replaced_during_renewal_does_not_leak_the_stale_identity_or_token() {
    let fixture = Fixture::new(false);
    let mut quota = fixture.quota_command();
    quota
        .env("TEST_PAUSE_OAUTH", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ReapedChild(Some(quota.spawn().unwrap()));
    wait_for(&fixture.root.path().join("oauth-started"));

    std::fs::write(
        fixture.slot.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "replacement-user",
                "organizationUuid": "replacement-org",
                "emailAddress": "replacement@example.com"
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        fixture.slot.join(".credentials.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "REPLACEMENT-ACCESS",
                "refreshToken": "REPLACEMENT-REFRESH",
                "expiresAt": 9_000_000_000_000_i64
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(fixture.root.path().join("oauth-release"), b"go").unwrap();
    let value = fixture.output(child.wait_with_output());
    let row = fixture.row(&value);

    assert_eq!(row["status"], "offline", "{row}");
    assert_eq!(row["email"], "replacement@example.com", "{row}");
    assert!(
        row["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("already being renewed")),
        "replacement deferral was hidden: {row}"
    );
    assert_eq!(fixture.calls("oauth-calls"), 1, "OAuth retried");
    assert_eq!(
        fixture.calls("old-usage-calls"),
        0,
        "stale access token used"
    );
    assert_eq!(
        fixture.calls("new-usage-calls"),
        0,
        "discarded response used"
    );
    assert_eq!(
        fixture.calls("replacement-usage-calls"),
        0,
        "replacement account was queried under the old row"
    );
}

#[test]
fn saved_profile_quota_renews_and_reads_its_authoritative_slot() {
    let fixture = Fixture::new(true);

    let value = fixture.quota();

    fixture.assert_first_read_used_renewal(&value);
}
