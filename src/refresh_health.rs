//! Persistent evidence that a Codex refresh credential was rejected.
//!
//! The marker is deliberately separate from `auth.json`: observing refresh
//! health must never rewrite credentials. It is bound to the account and
//! refresh token by a one-way fingerprint, so replacing either credential
//! makes old evidence irrelevant while an access-token-only update does not.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{anyhow, Result};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::secret::Secret;

const STATUS_FILE: &str = ".swapdex-refresh-status.json";
const STATUS_VERSION: u8 = 1;
const STATUS_TYPE: &str = "refresh_rejection";
const PROVIDER: &str = "codex";
const MAX_STATUS_BYTES: u64 = 4 * 1024;
const MAX_AUTH_BYTES: u64 = 2 * 1024 * 1024;

/// A credential field that is neither printable nor serializable and is
/// zeroized with the source [`Secret`] once the digest has been calculated.
struct SecretText(String);

impl<'de> Deserialize<'de> for SecretText {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Deserialize)]
struct CodexAuth {
    tokens: CredentialFields,
}

#[derive(Deserialize)]
struct CredentialFields {
    account_id: SecretText,
    refresh_token: SecretText,
}

#[derive(Serialize)]
struct StatusToWrite<'a> {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    provider: &'static str,
    rejected_at_ms: i64,
    credential_fingerprint: &'a str,
}

#[derive(Deserialize)]
struct StoredStatus {
    version: u8,
    #[serde(rename = "type")]
    kind: String,
    provider: String,
    rejected_at_ms: i64,
    credential_fingerprint: String,
}

/// Return a stable, non-secret digest of the Codex refresh credential in
/// `dir/auth.json`.
///
/// The digest covers a domain separator, provider, account id, and refresh
/// token using explicit length prefixes. Missing, malformed, oversized, or
/// symlinked credentials return `None`.
pub fn codex_credential_fingerprint(dir: &Path) -> Option<String> {
    let blob = Secret::new(read_bounded_regular(
        &dir.join("auth.json"),
        MAX_AUTH_BYTES,
    )?);
    codex_credential_fingerprint_from_blob(blob.expose())
}

/// Return the credential fingerprint from the blob already selected for a
/// refresh request.
///
/// Refresh must bind a server verdict to the account and refresh token it
/// actually sent. Reading `auth.json` again before the request could instead
/// fingerprint a concurrent login replacement and attach the old request's
/// failure to the new credential.
pub fn codex_credential_fingerprint_from_blob(blob: &[u8]) -> Option<String> {
    let auth: CodexAuth = serde_json::from_slice(blob).ok()?;
    if auth.tokens.account_id.0.is_empty() || auth.tokens.refresh_token.0.is_empty() {
        return None;
    }

    let mut hasher = Sha256::new();
    for part in [
        b"swapdex-refresh-health-v1".as_slice(),
        PROVIDER.as_bytes(),
        auth.tokens.account_id.0.as_bytes(),
        auth.tokens.refresh_token.0.as_bytes(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(fingerprint, "{byte:02x}").ok()?;
    }
    Some(fingerprint)
}

/// Record a definitive server rejection for the credential fingerprint the
/// caller observed before making its refresh request.
///
/// The current credential is re-read while refresh-health mutations are
/// serialized. If it no longer matches `expected_fingerprint`, this is a
/// successful no-op: a failed request using an old token must not mark a newer
/// login as rejected.
pub fn record_codex_rejection(dir: &Path, expected_fingerprint: &str, now_ms: i64) -> Result<()> {
    let Some(_lock) =
        lock_directory(dir).map_err(|_| anyhow!("could not persist Codex refresh rejection"))?
    else {
        return Ok(());
    };
    if codex_credential_fingerprint(dir).as_deref() != Some(expected_fingerprint) {
        return Ok(());
    }
    let rejected_at_ms = match read_status(dir)
        .filter(|status| status.credential_fingerprint == expected_fingerprint)
    {
        Some(status) if status.rejected_at_ms > now_ms => return Ok(()),
        // Millisecond timestamps can tie even when this rejection happened
        // after an already-running successful request. Preserve that order so
        // the earlier success cannot clear the later verdict.
        Some(status) if status.rejected_at_ms == now_ms => status.rejected_at_ms.saturating_add(1),
        _ => now_ms,
    };

    let status = StatusToWrite {
        version: STATUS_VERSION,
        kind: STATUS_TYPE,
        provider: PROVIDER,
        rejected_at_ms,
        credential_fingerprint: expected_fingerprint,
    };
    let bytes = serde_json::to_vec_pretty(&status)
        .map_err(|_| anyhow!("could not persist Codex refresh rejection"))?;
    crate::atomic::write_secret(&dir.join(STATUS_FILE), &bytes)
        .map_err(|_| anyhow!("could not persist Codex refresh rejection"))
}

/// Return when the current Codex refresh credential was definitively rejected.
///
/// Evidence for any other account or refresh token is stale and returns
/// `None`. Missing, malformed, oversized, or symlinked files also return
/// `None`; marker contents are never included in an error.
pub fn codex_rejection(dir: &Path) -> Option<i64> {
    let current = codex_credential_fingerprint(dir)?;
    codex_rejection_for_fingerprint(dir, &current)
}

/// Read rejection evidence for a credential snapshot already held by a caller.
/// This never rereads auth.json and cannot attach an old rejection to a newer
/// native login selected while another process is replacing the file.
pub(crate) fn codex_rejection_for_fingerprint(dir: &Path, fingerprint: &str) -> Option<i64> {
    let status = read_status(dir)?;
    (status.credential_fingerprint == fingerprint).then_some(status.rejected_at_ms)
}

/// Clear rejection evidence after a successful request made with
/// `expected_fingerprint`.
///
/// Only a marker bearing that fingerprint is removed. Mutations use the same
/// directory lock as [`record_codex_rejection`], so a late success for an old
/// credential cannot remove newer evidence for a different credential.
pub fn clear_codex_rejection(dir: &Path, expected_fingerprint: &str) -> Result<()> {
    clear_codex_rejection_inner(dir, expected_fingerprint, None)
}

/// Clear matching evidence only when it is no newer than the request that
/// succeeded.
///
/// A response can arrive after another process has recorded a later rejection
/// for the same credential. That stale success must not erase the newer
/// observation merely because both requests used the same refresh token.
pub fn clear_codex_rejection_before(
    dir: &Path,
    expected_fingerprint: &str,
    request_started_at_ms: i64,
) -> Result<()> {
    clear_codex_rejection_inner(dir, expected_fingerprint, Some(request_started_at_ms))
}

fn clear_codex_rejection_inner(
    dir: &Path,
    expected_fingerprint: &str,
    not_newer_than_ms: Option<i64>,
) -> Result<()> {
    let Some(_lock) =
        lock_directory(dir).map_err(|_| anyhow!("could not clear Codex refresh rejection"))?
    else {
        return Ok(());
    };
    let Some(status) = read_status(dir) else {
        return Ok(());
    };
    if status.credential_fingerprint != expected_fingerprint
        || not_newer_than_ms.is_some_and(|limit| status.rejected_at_ms > limit)
    {
        return Ok(());
    }

    match std::fs::remove_file(dir.join(STATUS_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(anyhow!("could not clear Codex refresh rejection")),
    }
}

fn read_status(dir: &Path) -> Option<StoredStatus> {
    let bytes = read_bounded_regular(&dir.join(STATUS_FILE), MAX_STATUS_BYTES)?;
    let status: StoredStatus = serde_json::from_slice(&bytes).ok()?;
    if status.version != STATUS_VERSION
        || status.kind != STATUS_TYPE
        || status.provider != PROVIDER
        || !valid_fingerprint(&status.credential_fingerprint)
    {
        return None;
    }
    Some(status)
}

fn valid_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 64
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Open a regular file without following a final symlink and cap the read even
/// if another writer grows the file after the metadata check.
fn read_bounded_regular(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return None;
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= max_bytes).then_some(bytes)
}

/// `flock` on the config directory leaves no extra lock artifact beside the
/// one specified sidecar. All refresh-health writers take this lock before
/// their compare-and-mutate sequence.
fn lock_directory(dir: &Path) -> std::io::Result<Option<std::fs::File>> {
    let directory = match std::fs::File::open(dir) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    FileExt::lock_exclusive(&directory)?;
    Ok(Some(directory))
}
