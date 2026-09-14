use std::path::Path;

use swapdex::refresh_health::{
    clear_codex_rejection, clear_codex_rejection_before, codex_credential_fingerprint,
    codex_credential_fingerprint_from_blob, codex_rejection, record_codex_rejection,
};

const STATUS_FILE: &str = ".swapdex-refresh-status.json";

fn auth_bytes(account: &str, refresh: &str, access: &str) -> Vec<u8> {
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

fn write_auth(dir: &Path, account: &str, refresh: &str, access: &str) -> Vec<u8> {
    let bytes = auth_bytes(account, refresh, access);
    std::fs::write(dir.join("auth.json"), &bytes).unwrap();
    bytes
}

#[test]
fn in_memory_fingerprint_matches_the_exact_blob_written_to_disk() {
    let temp = tempfile::tempdir().unwrap();
    let blob = write_auth(temp.path(), "account-a", "refresh-a", "access-a");

    assert_eq!(
        codex_credential_fingerprint_from_blob(&blob),
        codex_credential_fingerprint(temp.path())
    );
}

#[test]
fn rejected_current_credential_is_visible_without_touching_auth() {
    let temp = tempfile::tempdir().unwrap();
    let before = write_auth(temp.path(), "account-old", "refresh-old", "access-old");
    let fingerprint = codex_credential_fingerprint(temp.path()).expect("credential fingerprint");

    record_codex_rejection(temp.path(), &fingerprint, 1_800_000_000_123).unwrap();

    assert_eq!(codex_rejection(temp.path()), Some(1_800_000_000_123));
    assert_eq!(
        std::fs::read(temp.path().join("auth.json")).unwrap(),
        before,
        "recording health metadata must not rewrite credentials"
    );
}

#[test]
fn marker_contains_metadata_and_digest_but_no_credential_material() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(
        temp.path(),
        "account-sentinel",
        "refresh-sentinel",
        "access-sentinel",
    );
    let fingerprint = codex_credential_fingerprint(temp.path()).unwrap();

    record_codex_rejection(temp.path(), &fingerprint, 44).unwrap();

    let marker = std::fs::read_to_string(temp.path().join(STATUS_FILE)).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(parsed["version"], 1);
    assert_eq!(parsed["type"], "refresh_rejection");
    assert_eq!(parsed["provider"], "codex");
    assert_eq!(parsed["rejected_at_ms"], 44);
    assert_eq!(parsed["credential_fingerprint"], fingerprint);
    for secret in [
        "account-sentinel",
        "refresh-sentinel",
        "access-sentinel",
        "synthetic-identity",
    ] {
        assert!(!marker.contains(secret), "sidecar exposed {secret}");
    }
}

#[test]
fn rotated_refresh_token_suppresses_stale_rejection() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let old = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &old, 100).unwrap();

    write_auth(temp.path(), "account-a", "refresh-b", "access-b");

    assert_eq!(codex_rejection(temp.path()), None);
}

#[test]
fn changed_account_suppresses_stale_rejection() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let old = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &old, 100).unwrap();

    write_auth(temp.path(), "account-b", "refresh-a", "access-b");

    assert_eq!(codex_rejection(temp.path()), None);
}

#[test]
fn access_only_change_keeps_definitive_refresh_rejection_visible() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let fingerprint = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &fingerprint, 101).unwrap();

    let mut changed: serde_json::Value =
        serde_json::from_slice(&auth_bytes("account-a", "refresh-a", "access-b")).unwrap();
    changed["last_refresh"] = "2030-01-01T00:00:00Z".into();
    std::fs::write(
        temp.path().join("auth.json"),
        serde_json::to_vec(&changed).unwrap(),
    )
    .unwrap();

    assert_eq!(codex_rejection(temp.path()), Some(101));
}

#[test]
fn failed_old_request_cannot_mark_new_credential() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let observed_before_request = codex_credential_fingerprint(temp.path()).unwrap();

    write_auth(temp.path(), "account-b", "refresh-b", "access-b");
    record_codex_rejection(temp.path(), &observed_before_request, 102).unwrap();

    assert_eq!(codex_rejection(temp.path()), None);
    assert!(!temp.path().join(STATUS_FILE).exists());
}

#[test]
fn successful_expected_credential_clears_matching_evidence() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let fingerprint = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &fingerprint, 103).unwrap();

    clear_codex_rejection(temp.path(), &fingerprint).unwrap();

    assert_eq!(codex_rejection(temp.path()), None);
    assert!(!temp.path().join(STATUS_FILE).exists());
}

#[test]
fn old_success_cannot_clear_newer_credentials_rejection() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let old = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &old, 104).unwrap();

    write_auth(temp.path(), "account-b", "refresh-b", "access-b");
    let new = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &new, 105).unwrap();
    clear_codex_rejection(temp.path(), &old).unwrap();

    assert_eq!(codex_rejection(temp.path()), Some(105));
}

#[test]
fn stale_success_cannot_clear_a_newer_rejection_for_the_same_credential() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let fingerprint = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &fingerprint, 200).unwrap();

    clear_codex_rejection_before(temp.path(), &fingerprint, 100).unwrap();

    assert_eq!(codex_rejection(temp.path()), Some(200));
}

#[test]
fn stale_rejection_cannot_move_a_matching_marker_backwards() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let fingerprint = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &fingerprint, 200).unwrap();

    record_codex_rejection(temp.path(), &fingerprint, 100).unwrap();

    assert_eq!(codex_rejection(temp.path()), Some(200));
}

#[test]
fn equal_millisecond_rejection_survives_a_success_that_was_already_running() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    let fingerprint = codex_credential_fingerprint(temp.path()).unwrap();
    record_codex_rejection(temp.path(), &fingerprint, 200).unwrap();

    record_codex_rejection(temp.path(), &fingerprint, 200).unwrap();
    clear_codex_rejection_before(temp.path(), &fingerprint, 200).unwrap();

    assert_eq!(codex_rejection(temp.path()), Some(201));
}

#[test]
fn malformed_or_missing_auth_and_sidecar_are_safe() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(codex_credential_fingerprint(temp.path()), None);
    assert_eq!(codex_rejection(temp.path()), None);

    std::fs::write(temp.path().join("auth.json"), b"not json").unwrap();
    std::fs::write(temp.path().join(STATUS_FILE), b"also not json").unwrap();
    assert_eq!(codex_credential_fingerprint(temp.path()), None);
    assert_eq!(codex_rejection(temp.path()), None);
    record_codex_rejection(temp.path(), "not-a-current-fingerprint", 106).unwrap();
    clear_codex_rejection(temp.path(), "not-a-current-fingerprint").unwrap();
}

#[test]
fn oversized_sidecar_is_ignored() {
    let temp = tempfile::tempdir().unwrap();
    write_auth(temp.path(), "account-a", "refresh-a", "access-a");
    std::fs::write(temp.path().join(STATUS_FILE), vec![b'x'; 32 * 1024]).unwrap();

    assert_eq!(codex_rejection(temp.path()), None);
}

#[test]
fn fingerprint_encoding_keeps_field_boundaries_unambiguous() {
    let left = tempfile::tempdir().unwrap();
    write_auth(left.path(), "a", "bc", "access-a");
    let right = tempfile::tempdir().unwrap();
    write_auth(right.path(), "ab", "c", "access-b");

    assert_ne!(
        codex_credential_fingerprint(left.path()),
        codex_credential_fingerprint(right.path())
    );
}

#[cfg(unix)]
#[test]
fn persistence_error_does_not_expose_paths_or_credentials() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write_auth(
        temp.path(),
        "account-error-sentinel",
        "refresh-error-sentinel",
        "access-error-sentinel",
    );
    let fingerprint = codex_credential_fingerprint(temp.path()).unwrap();
    let outside = temp.path().join("outside");
    std::fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, temp.path().join(STATUS_FILE)).unwrap();

    let message = record_codex_rejection(temp.path(), &fingerprint, 107)
        .unwrap_err()
        .to_string();

    assert_eq!(message, "could not persist Codex refresh rejection");
    for exposed in [
        temp.path().to_string_lossy().as_ref(),
        "account-error-sentinel",
        "refresh-error-sentinel",
        "access-error-sentinel",
    ] {
        assert!(
            !message.contains(exposed),
            "error exposed sensitive context"
        );
    }
    assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
}
