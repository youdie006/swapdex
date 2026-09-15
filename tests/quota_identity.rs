//! A same-name saved profile must not substitute its login for a current slot.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn quota_uses_only_the_current_slots_identity_and_credential() {
    for (readable_slot, live_profile, expired_slot, saved_profile) in [
        (false, false, false, true),
        (false, true, false, true),
        (true, true, false, true),
        (true, true, true, true),
        (false, false, false, false),
        (false, true, false, false),
        (true, true, false, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join(".local/share/swapdex");
        let slot = store.join("slots/current");
        let saved = store.join("accounts/work/claude-code");
        std::fs::create_dir_all(&slot).unwrap();
        let old_id =
            serde_json::json!({"accountUuid":"old-user", "emailAddress":"old@example.test"});
        let slot_id = serde_json::json!({"accountUuid":"current-user", "emailAddress":"current@example.test"});
        let credential = |access: &str| {
            serde_json::json!({"claudeAiOauth":{
                "accessToken":access, "expiresAt":9_000_000_000_000_i64
            }})
            .to_string()
        };
        if saved_profile {
            std::fs::create_dir_all(&saved).unwrap();
            std::fs::write(saved.join("oauth_account"), old_id.to_string()).unwrap();
            std::fs::write(saved.join("credentials"), credential("OLD-PROFILE-ACCESS")).unwrap();
        }
        std::fs::write(
            slot.join(".claude.json"),
            serde_json::json!({"oauthAccount":slot_id}).to_string(),
        )
        .unwrap();
        if readable_slot {
            let mut current: serde_json::Value =
                serde_json::from_str(&credential("CURRENT-SLOT-ACCESS")).unwrap();
            if expired_slot {
                current["claudeAiOauth"]["expiresAt"] = 1.into();
            }
            std::fs::write(slot.join(".credentials.json"), current.to_string()).unwrap();
        }
        if live_profile {
            std::fs::create_dir_all(root.path().join(".claude")).unwrap();
            std::fs::write(
                root.path().join(".claude/.credentials.json"),
                credential("OLD-LIVE-ACCESS"),
            )
            .unwrap();
            std::fs::write(
                root.path().join(".claude.json"),
                serde_json::json!({"oauthAccount":old_id}).to_string(),
            )
            .unwrap();
        }
        std::fs::write(store.join("slots.json"), serde_json::json!([{
            "name":"work", "id":"current", "tool":"claude-code", "adopted":false, "config_dir":slot
        }]).to_string()).unwrap();
        std::fs::write(store.join("active-claude"), slot.to_str().unwrap()).unwrap();
        let curl = root.path().join("fake-curl");
        std::fs::write(
            &curl,
            r#"#!/bin/sh
cfg=$(cat)
case "$cfg" in
  *OLD-PROFILE-ACCESS*) printf x >> "$HOME/old-profile-used"; usage=99 ;;
  *OLD-LIVE-ACCESS*) printf x >> "$HOME/old-live-used"; usage=31 ;;
  *CURRENT-SLOT-ACCESS*) printf x >> "$HOME/current-slot-used"; usage=12 ;;
  *) exit 91 ;;
esac
printf '{"five_hour":{"utilization":%s}}\n200' "$usage"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_swapdex"))
            .args(["quota", "--json"])
            .env("SWAPDEX_ROOT", root.path())
            .env("HOME", root.path())
            .env("SWAPDEX_CURL", curl)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let row = value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "work")
            .unwrap();
        assert!(
            !root.path().join("old-profile-used").exists(),
            "unreadable slot used a different saved login"
        );
        assert_eq!(row["email"], "current@example.test", "{row}");
        assert_eq!(row["active"], true, "{row}");
        let native = value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "(active login, not saved)");
        assert_eq!(
            native.is_some(),
            live_profile,
            "unrelated native login was hidden: {value}"
        );
        assert_eq!(root.path().join("old-live-used").exists(), live_profile);
        if let Some(native) = native {
            assert_eq!(native["five_hour"]["used_pct"], 31.0, "{native}");
        }
        assert_eq!(
            root.path().join("current-slot-used").exists(),
            readable_slot && !expired_slot,
            "{row}"
        );
        if !readable_slot || expired_slot {
            assert!(
                row["five_hour"].is_null(),
                "unreadable slot displayed another login's quota: {row}"
            );
        } else {
            assert_eq!(row["five_hour"]["used_pct"], 12.0, "{row}");
        }
    }
}
