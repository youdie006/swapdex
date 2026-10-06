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
            serde_json::json!({"accountUuid":"old-user", "emailAddress":"old@example.com"});
        let slot_id =
            serde_json::json!({"accountUuid":"current-user", "emailAddress":"current@example.com"});
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
        assert_eq!(row["email"], "current@example.com", "{row}");
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

/// Codex rows name the plan beside the address (`[pro]`); Claude rows named
/// nothing in `quota`, and `ls` said only `max` - which Max, 5x or 20x, is in
/// the same credential as `rateLimitTier`.
#[test]
fn claude_rows_name_the_plan_and_its_max_tier() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots/big");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        serde_json::json!({"oauthAccount":{"accountUuid":"u-big","emailAddress":"big@example.com"}})
            .to_string(),
    )
    .unwrap();
    let credential = serde_json::json!({"claudeAiOauth":{
        "accessToken":"BIG-ACCESS", "refreshToken":"BIG-RT", "expiresAt":9_000_000_000_000_i64,
        "subscriptionType":"max", "rateLimitTier":"default_claude_max_20x"}})
    .to_string();
    std::fs::write(slot.join(".credentials.json"), &credential).unwrap();
    std::fs::set_permissions(
        slot.join(".credentials.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(
        store.join("slots.json"),
        serde_json::json!([{
            "name":"big", "id":"big", "tool":"claude-code", "adopted":false, "config_dir":slot
        }])
        .to_string(),
    )
    .unwrap();
    let curl = root.path().join("fake-curl");
    std::fs::write(
        &curl,
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"five_hour\":{\"utilization\":12}}\\n200'\n",
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let swapdex = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_swapdex"))
            .args(args)
            .env("SWAPDEX_ROOT", root.path())
            .env("HOME", root.path())
            .env("SWAPDEX_CURL", &curl)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let quota = swapdex(&["quota"]);
    assert!(
        quota
            .lines()
            .any(|l| l.starts_with("big") && l.contains("big@example.com [max 20x]")),
        "quota does not name the plan:\n{quota}"
    );
    let listing = swapdex(&["ls"]);
    assert!(
        listing.contains("big@example.com [max 20x]"),
        "ls does not name the Max tier:\n{listing}"
    );
}

/// Keep-alive renews a Claude login near its refresh token's end, so an idle
/// account's eight-hour access token lapses between renewals while the login
/// stays good - the proxy renews it the moment it is used. `quota` called that
/// state "slot access expired - renewal has not completed ... sign in",
/// which reads as the login being lost.
#[test]
fn an_idle_claude_slot_is_not_reported_as_a_failed_renewal() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots/idle");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        serde_json::json!({"oauthAccount":{"accountUuid":"u-idle","emailAddress":"idle@example.com"}})
            .to_string(),
    )
    .unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let credential = serde_json::json!({"claudeAiOauth":{
        "accessToken":"IDLE-ACCESS", "refreshToken":"IDLE-RT", "expiresAt": now_ms - 3_600_000,
        "refreshTokenExpiresAt": now_ms + 20 * 86_400_000i64, "subscriptionType":"max"}})
    .to_string();
    std::fs::write(slot.join(".credentials.json"), &credential).unwrap();
    std::fs::set_permissions(
        slot.join(".credentials.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(
        store.join("slots.json"),
        serde_json::json!([{
            "name":"idle", "id":"idle", "tool":"claude-code", "adopted":false, "config_dir":slot
        }])
        .to_string(),
    )
    .unwrap();
    let curl = root.path().join("fake-curl");
    std::fs::write(&curl, "#!/bin/sh\nexit 91\n").unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_swapdex"))
        .arg("quota")
        .env("SWAPDEX_ROOT", root.path())
        .env("HOME", root.path())
        .env("SWAPDEX_CURL", &curl)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !said.contains("renewal has not completed") && !said.contains("sign in"),
        "an idle login was reported as a failed renewal:\n{said}"
    );
    assert!(
        said.contains("renews on next use"),
        "quota does not say the idle login is fine:\n{said}"
    );

    // The over-correction: a refresh token past its end cannot renew, and
    // must still be reported as needing a sign-in.
    let dead = serde_json::json!({"claudeAiOauth":{
        "accessToken":"IDLE-ACCESS", "refreshToken":"IDLE-RT", "expiresAt": now_ms - 3_600_000,
        "refreshTokenExpiresAt": now_ms - 60_000, "subscriptionType":"max"}})
    .to_string();
    std::fs::write(slot.join(".credentials.json"), &dead).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_swapdex"))
        .arg("quota")
        .env("SWAPDEX_ROOT", root.path())
        .env("HOME", root.path())
        .env("SWAPDEX_CURL", &curl)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !said.contains("renews on next use") && said.contains("sign in"),
        "a login past its refresh token's end was called idle:\n{said}"
    );
}
