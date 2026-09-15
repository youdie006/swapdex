use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::{Command, Output};

#[cfg(target_os = "linux")]
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn seed_claude(config_dir: &Path, identity_path: &Path, tag: &str) {
    std::fs::create_dir_all(config_dir).unwrap();
    std::fs::write(
        config_dir.join(".credentials.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("AT-{tag}"),
                "refreshToken": format!("RT-{tag}"),
                "expiresAt": 9999999999999i64
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        identity_path,
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": format!("UUID-{tag}"),
                "emailAddress": format!("{}@example.com", tag.to_ascii_lowercase())
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
fn capture(home: &Path, data: &Path, claude_config_dir: Option<&Path>) -> Output {
    let mut command = Command::new(bin());
    command
        .args(["add", "captured", "--tool", "claude"])
        .current_dir(home)
        .env_remove("SWAPDEX_ROOT")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .env("HOME", home)
        .env("XDG_DATA_HOME", data);
    if let Some(dir) = claude_config_dir {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    command.output().unwrap()
}

#[cfg(target_os = "linux")]
fn saved_identity(data: &Path) -> serde_json::Value {
    let bytes =
        std::fs::read(data.join("swapdex/accounts/captured/claude-code/oauth_account")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
#[cfg(target_os = "linux")]
fn implicit_default_reads_identity_from_the_home_sibling() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    seed_claude(&home.join(".claude"), &home.join(".claude.json"), "HOME");

    let output = capture(&home, &data, None);

    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(saved_identity(&data)["accountUuid"], "UUID-HOME");
}

#[test]
#[cfg(target_os = "linux")]
fn explicit_custom_config_reads_identity_inside_that_config() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let data = temp.path().join("data");
    let custom = home.join(".claude-work");
    std::fs::create_dir_all(&home).unwrap();
    seed_claude(&home.join(".claude"), &home.join(".claude.json"), "HOME");
    seed_claude(&custom, &custom.join(".claude.json"), "CUSTOM");

    let output = capture(&home, &data, Some(&custom));

    assert!(output.status.success());
    assert_eq!(saved_identity(&data)["accountUuid"], "UUID-CUSTOM");
}

#[test]
#[cfg(target_os = "linux")]
fn explicitly_naming_the_default_config_still_uses_its_local_identity() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let data = temp.path().join("data");
    let default_config = home.join(".claude");
    std::fs::create_dir_all(&home).unwrap();
    seed_claude(&default_config, &home.join(".claude.json"), "IMPLICIT");
    std::fs::write(
        default_config.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "UUID-EXPLICIT",
                "emailAddress": "explicit@example.com"
            }
        })
        .to_string(),
    )
    .unwrap();

    let output = capture(&home, &data, Some(&default_config));

    assert!(output.status.success());
    assert_eq!(saved_identity(&data)["accountUuid"], "UUID-EXPLICIT");
}

#[test]
#[cfg(target_os = "linux")]
fn an_empty_config_override_keeps_the_implicit_default_layout() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    seed_claude(&home.join(".claude"), &home.join(".claude.json"), "HOME");

    let output = capture(&home, &data, Some(Path::new("")));

    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(saved_identity(&data)["accountUuid"], "UUID-HOME");
}

#[test]
fn rooted_default_capture_and_apply_are_file_only() {
    let source = tempfile::tempdir().unwrap();
    let source_paths = swapdex::paths::Paths::rooted(source.path());
    seed_claude(
        source_paths.claude_dir(),
        &source_paths.claude_config_json(),
        "SOURCE",
    );
    let snapshot = swapdex::adapters::by_name("claude-code")
        .unwrap()
        .capture(&source_paths)
        .unwrap();

    let target = tempfile::tempdir().unwrap();
    let target_paths = swapdex::paths::Paths::rooted(target.path());
    seed_claude(
        target_paths.claude_dir(),
        &target_paths.claude_config_json(),
        "TARGET",
    );
    swapdex::adapters::by_name("claude-code")
        .unwrap()
        .apply(&target_paths, &snapshot)
        .unwrap();

    let credentials: serde_json::Value =
        serde_json::from_slice(&std::fs::read(target_paths.claude_credentials()).unwrap()).unwrap();
    let identity: serde_json::Value =
        serde_json::from_slice(&std::fs::read(target_paths.claude_config_json()).unwrap()).unwrap();
    assert_eq!(credentials["claudeAiOauth"]["accessToken"], "AT-SOURCE");
    assert_eq!(identity["oauthAccount"]["accountUuid"], "UUID-SOURCE");
}

#[test]
fn a_rooted_slot_capture_stays_inside_its_sandbox() {
    let temp = tempfile::tempdir().unwrap();
    let paths = swapdex::paths::Paths::rooted(temp.path());
    seed_claude(paths.claude_dir(), &paths.claude_config_json(), "DEFAULT");
    let slot = temp.path().join("slot");
    seed_claude(&slot, &slot.join(".claude.json"), "SLOT");

    let at_slot = paths.with_tool_dir("claude-code", &slot);
    let snapshot = swapdex::adapters::by_name("claude-code")
        .unwrap()
        .capture(&at_slot)
        .unwrap();
    let credentials: serde_json::Value =
        serde_json::from_slice(snapshot.part("credentials").unwrap().expose()).unwrap();
    let identity: serde_json::Value =
        serde_json::from_slice(snapshot.part("oauth_account").unwrap().expose()).unwrap();

    assert_eq!(credentials["claudeAiOauth"]["accessToken"], "AT-SLOT");
    assert_eq!(identity["accountUuid"], "UUID-SLOT");
}

#[test]
fn a_missing_rooted_slot_credential_does_not_borrow_the_default() {
    let temp = tempfile::tempdir().unwrap();
    let paths = swapdex::paths::Paths::rooted(temp.path());
    seed_claude(paths.claude_dir(), &paths.claude_config_json(), "DEFAULT");
    let slot = temp.path().join("slot");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"UUID-SLOT","emailAddress":"slot@example.com"}}"#,
    )
    .unwrap();

    let at_slot = paths.with_tool_dir("claude-code", &slot);
    let error = match swapdex::adapters::by_name("claude-code")
        .unwrap()
        .capture(&at_slot)
    {
        Ok(_) => panic!("capture borrowed the default credential"),
        Err(error) => error,
    };

    assert!(
        error.to_string().contains("selected Claude slot"),
        "{error:#}"
    );
}
