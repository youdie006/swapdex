//! Renewing a slot's access token.
//!
//! An access token lives about an hour, and only the account's own refresh token
//! can renew it. Until now swapdex would not do that - it read credentials and
//! never wrote them - so any account idle for an hour looked expired: the proxy
//! stepped over it and `quota` reported it dead. Those are exactly the accounts
//! with quota left, which made the tool least useful precisely when it was needed.
//!
//! Refreshing means writing a credential, and that is a line worth naming. Two
//! properties make it safe to cross:
//!
//! 1. **Never while the tool is running there.** A refresh token ROTATES: the
//!    server issues a new one and retires the old. A Claude running in that slot
//!    holds the old one in memory, and when it later refreshes with a token that
//!    has already been spent, the server can revoke the whole chain - which is
//!    the logout this project exists to prevent. So a slot in use is never
//!    touched.
//! 2. **The new credential replaces the old one in place**, in whichever store
//!    the account already keeps it, so the tool's next run reads what swapdex
//!    wrote rather than a second, competing copy.
//!
//! No token value is ever logged, and the request carries it on curl's stdin -
//! the same discipline `quota` uses, so it never reaches `ps`.

use crate::{paths::Paths, secret::Secret};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Where an OAuth refresh is exchanged. `SWAPDEX_OAUTH_URL` redirects it for
/// tests, honored ONLY under `SWAPDEX_ROOT` so a production run can never be
/// pointed at another host with a live refresh token.
pub fn token_url() -> String {
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        if let Some(u) = std::env::var_os("SWAPDEX_OAUTH_URL") {
            return u.to_string_lossy().into_owned();
        }
    }
    "https://console.anthropic.com/v1/oauth/token".to_string()
}

/// Claude Code's public OAuth client, as its own authorize URL carries it.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Why a refresh did not happen. Each is a different thing to tell the user, and
/// none of them should read as "your account is gone".
#[derive(Clone, Debug, PartialEq)]
pub enum RefreshError {
    /// The slot has no readable credential to renew.
    NoCredential,
    /// The tool is running in this slot right now. Refreshing would retire the
    /// refresh token it holds in memory, and its next renewal would fail.
    InUse,
    /// The refresh token itself has expired - only a fresh sign-in fixes that.
    Expired,
    /// The login server is rate-limiting; the account itself is fine.
    Busy,
    /// Another caller may have renewed this account, but its exact result could
    /// not be established safely. Spending the same refresh token twice is what
    /// logs an account out, so ambiguity remains a conservative stand-down.
    AlreadyRefreshing,
    /// The server refused the exchange.
    Refused(String),
    /// The request could not be made at all.
    Offline(String),
}

/// A successful refresh request either rotated the saved credential or proved
/// that the native client already owns a usable access token for this account.
/// Keeping those outcomes separate prevents a no-op from being reported as an
/// OAuth exchange and credential write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
    Renewed,
    NativeManaged,
}

type RefreshResult = Result<RefreshOutcome, RefreshError>;

impl RefreshError {
    /// What the user should do about it, in one line.
    ///
    /// `run` defaults to Claude, so every remedy naming it must name the tool
    /// too - otherwise a dead Codex account is told to launch Claude.
    pub fn remedy(&self, name: &str, tool: &str) -> String {
        let flag = match tool {
            "claude-code" | "claude" => String::new(),
            other => format!(" --tool {other}"),
        };
        match self {
            Self::NoCredential => {
                format!("'{name}' has no login yet - `swapdex run {name}{flag}` signs it in")
            }
            Self::InUse => format!(
                "'{name}' {tool} renewal deferred - refresh unverified while a {tool} session \
                 is using this account; retry after the session exits"
            ),
            Self::Expired => format!(
                "'{name}' has been idle too long to renew - \
                 `swapdex run {name}{flag}` signs it in again"
            ),
            Self::Busy => format!(
                "the login server is busy - '{name}' is fine, renewing again shortly will work"
            ),
            Self::AlreadyRefreshing => format!(
                "'{name}' is already being renewed by another turn - nothing to do; \
                 spending its refresh token twice is what logs an account out"
            ),
            Self::Refused(why) => format!("'{name}' could not be renewed: {why}"),
            Self::Offline(why) => format!("could not reach the login server: {why}"),
        }
    }
}

/// Has the refresh token itself expired? Then nothing here can help and only a
/// sign-in will - saying so is the difference between a fixable state and a
/// mysterious one.
fn refresh_token_expired(blob: &[u8], now_ms: i64) -> bool {
    serde_json::from_slice::<serde_json::Value>(blob)
        .ok()
        .and_then(|v| v["claudeAiOauth"]["refreshTokenExpiresAt"].as_i64())
        .is_some_and(|exp| exp <= now_ms)
}

/// Build the renewed credential blob by merging the server's answer into the one
/// on disk, so fields swapdex does not understand survive untouched.
///
/// The new refresh token REPLACES the old one when the server sends it. Keeping
/// the old one would leave a spent token on disk and the account unusable on the
/// renewal after this one.
pub fn merge_response(old: &[u8], response: &str, now_ms: i64) -> Option<Vec<u8>> {
    let mut blob: serde_json::Value = serde_json::from_slice(old).ok()?;
    let r: serde_json::Value = serde_json::from_str(response).ok()?;
    let access = r["access_token"].as_str().filter(|s| !s.is_empty())?;
    let o = blob.get_mut("claudeAiOauth")?.as_object_mut()?;
    o.insert("accessToken".into(), access.into());
    if let Some(rt) = r["refresh_token"].as_str().filter(|s| !s.is_empty()) {
        o.insert("refreshToken".into(), rt.into());
        // The deadline on disk belonged to the token this one replaces. The
        // server retired that one, so the recorded moment now describes
        // something that does not exist - and it is exactly what decides
        // whether renewing is worth trying. Left in place it made every
        // renewal leave the account one day nearer a sign-in swapdex would
        // demand while holding a refresh token the server had just issued.
        //
        // Record the new lifetime when the server states it. When it does not,
        // drop the stale one rather than apply it to a different token: the
        // server is then the one that says no, which is the only party that
        // knows.
        match r["refresh_token_expires_in"].as_i64() {
            Some(secs) => {
                o.insert(
                    "refreshTokenExpiresAt".into(),
                    (now_ms + secs * 1000).into(),
                );
            }
            None => {
                o.remove("refreshTokenExpiresAt");
            }
        }
    }
    // `expires_in` is seconds from now; the file records an absolute moment.
    if let Some(secs) = r["expires_in"].as_i64() {
        o.insert("expiresAt".into(), (now_ms + secs * 1000).into());
    }
    serde_json::to_vec(&blob).ok()
}

/// The request body for an exchange. Kept separate so a test can assert its
/// shape without a network call.
pub fn request_body(refresh_token: &str) -> String {
    serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    })
    .to_string()
}

/// How close together two refreshes count as the same burst.
pub const BURST_SECS: i64 = 30;

/// Lets one leader refresh an account while followers wait for its result.
///
/// Refresh tokens rotate: each use mints a new one and retires the old. So N
/// concurrent turns each refreshing the same slot spend the same token N times,
/// and every result but one is already invalid when it lands - the account ends
/// up logged out by its own renewal. teamclaude hit this as "don't rotate the
/// token family once per 401 in a burst".
///
/// Per-account, so one account's burst never blocks another's genuine refresh,
/// and time-bounded, so a refresh minutes later is a new event rather than the
/// same burst still being suppressed.
const WAIT_FOR_REFRESH: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct CompletedRefresh {
    generation: String,
    result_generation: Option<String>,
    source: String,
    completed_at: Instant,
    result: RefreshResult,
}

enum GateEntry {
    Running {
        generation: String,
        source: String,
        started: Instant,
    },
    Complete(CompletedRefresh),
}

enum GateDecision {
    Leader,
    Shared {
        result: RefreshResult,
        result_generation: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChangedSuccessPolicy {
    Retry,
    Defer,
}

#[derive(Default)]
struct RefreshGate {
    entries: Mutex<HashMap<PathBuf, GateEntry>>,
    changed: Condvar,
}

impl RefreshGate {
    fn begin(
        &self,
        key: &Path,
        generation: &str,
        source: &str,
        changed_success: ChangedSuccessPolicy,
    ) -> GateDecision {
        let wait_started = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match entries.get(key) {
                None => {
                    entries.insert(
                        key.to_path_buf(),
                        GateEntry::Running {
                            generation: generation.to_string(),
                            source: source.to_string(),
                            started: Instant::now(),
                        },
                    );
                    return GateDecision::Leader;
                }
                Some(GateEntry::Running { started, .. }) => {
                    let elapsed = wait_started.elapsed();
                    if elapsed >= WAIT_FOR_REFRESH || started.elapsed() >= WAIT_FOR_REFRESH {
                        // The filesystem lock remains the final exclusion layer.
                        // Removing an abandoned local claim cannot overlap a live
                        // exchange in this or another process.
                        entries.remove(key);
                        self.changed.notify_all();
                        return GateDecision::Shared {
                            result: Err(RefreshError::AlreadyRefreshing),
                            result_generation: None,
                        };
                    }
                    let remaining = WAIT_FOR_REFRESH.saturating_sub(elapsed);
                    let waited = self
                        .changed
                        .wait_timeout(entries, remaining)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    entries = waited.0;
                }
                Some(GateEntry::Complete(completed)) => {
                    let recent =
                        completed.completed_at.elapsed() <= Duration::from_secs(BURST_SECS as u64);
                    if !recent {
                        entries.remove(key);
                        continue;
                    }
                    if completed.source != source {
                        // Two directories carrying one provider identity may be
                        // stale copies. They share exclusion, but never inherit a
                        // success that was persisted somewhere else.
                        return GateDecision::Shared {
                            result: Err(RefreshError::AlreadyRefreshing),
                            result_generation: None,
                        };
                    }
                    if completed.generation == generation
                        || completed.result_generation.as_deref() == Some(generation)
                    {
                        return GateDecision::Shared {
                            result: completed.result.clone(),
                            result_generation: completed.result_generation.clone(),
                        };
                    }
                    if changed_success == ChangedSuccessPolicy::Defer
                        && matches!(&completed.result, Ok(RefreshOutcome::Renewed))
                    {
                        // A successful exchange may have retired the shared
                        // rotating token even when its response did not return
                        // a replacement. A changed credential blob cannot claim
                        // that success, but retrying it could spend the token
                        // again.
                        return GateDecision::Shared {
                            result: Err(RefreshError::AlreadyRefreshing),
                            result_generation: None,
                        };
                    }
                    // A replacement generation after a failure is a new login,
                    // so an old refusal must not poison it.
                    entries.insert(
                        key.to_path_buf(),
                        GateEntry::Running {
                            generation: generation.to_string(),
                            source: source.to_string(),
                            started: Instant::now(),
                        },
                    );
                    return GateDecision::Leader;
                }
            }
        }
    }

    fn finish(
        &self,
        key: &Path,
        generation: &str,
        source: &str,
        result: Option<(RefreshResult, Option<String>)>,
    ) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owns_entry = matches!(
            entries.get(key),
            Some(GateEntry::Running {
                generation: current_generation,
                source: current_source,
                ..
            }) if current_generation == generation && current_source == source
        );
        if owns_entry {
            match result {
                Some((result, result_generation)) => {
                    entries.insert(
                        key.to_path_buf(),
                        GateEntry::Complete(CompletedRefresh {
                            generation: generation.to_string(),
                            result_generation,
                            source: source.to_string(),
                            completed_at: Instant::now(),
                            result,
                        }),
                    );
                }
                None => {
                    entries.remove(key);
                }
            }
        }
        self.changed.notify_all();
    }
}

/// The one gate every refresh passes through.
///
/// `RefreshGate` was created for this and then applied at a single call site,
/// while two other paths - the keep-alive sweep and `has_usable_login` - reached
/// `refresh_slot` unguarded. A rule enforced at one caller is a rule the next
/// caller does not know exists, so it lives here now, where the token is
/// actually spent.
fn gate() -> &'static RefreshGate {
    static GATE: OnceLock<RefreshGate> = OnceLock::new();
    GATE.get_or_init(RefreshGate::default)
}

/// The account a slot holds, however its tool records it.
/// The provider is part of the identity, not just the id under it.
///
/// A Claude `accountUuid` and a ChatGPT `account_id` are drawn from different
/// namespaces, and one person's email can hold a subscription to both. Keying
/// on the bare id would let two unrelated accounts share one claim - the same
/// correction KarpelesLab/teamclaude made in its own pool (#349).
fn identity_file(dir: &Path, name: &str) -> Option<Vec<u8>> {
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    std::fs::read(path).ok()
}

/// Stable, non-secret refresh ownership encoded in one credential blob.
///
/// Codex workspace ids identify the payer, but multiple users can belong to one
/// workspace and hold independent refresh-token families. A complete Codex
/// identity therefore includes the JWT subject, matching live-login selection.
/// Older or opaque credentials retain the workspace-only identity they used
/// before subjects were considered.
pub(crate) fn credential_identity(bytes: &[u8], tool: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match tool {
        "claude-code" => value["oauthAccount"]["accountUuid"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(|id| format!("claude:{id}")),
        "codex" => {
            let workspace = value["tokens"]["account_id"]
                .as_str()
                .filter(|id| !id.is_empty())?;
            match crate::live_login::identity_from_credential(bytes, "codex") {
                Some(crate::live_login::LoginIdentity::Codex {
                    subject,
                    workspace_id,
                }) => Some(format!(
                    "codex-user:{}:{subject}:{workspace_id}",
                    subject.len()
                )),
                _ => Some(format!("codex:{workspace}")),
            }
        }
        _ => None,
    }
}

/// Provider-qualified, non-secret identity for one rotating refresh token.
/// This joins stale credential copies even when one copy has richer account
/// metadata. The raw token is never used as a lock name or diagnostic.
pub(crate) fn refresh_token_identity(bytes: &[u8], tool: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let token = match tool {
        "claude-code" => value["claudeAiOauth"]["refreshToken"].as_str(),
        "codex" => value["tokens"]["refresh_token"].as_str(),
        _ => None,
    }
    .filter(|token| !token.is_empty())?;
    Some(refresh_token_identity_from_token(token, tool))
}

fn refresh_token_identity_from_token(token: &str, tool: &str) -> String {
    format!("refresh-token:{tool}:{}", fingerprint(token.as_bytes()))
}

fn account_from_identity(path: &Path, tool: &str) -> Option<String> {
    let parent = path.parent()?;
    let name = path.file_name()?.to_str()?;
    let bytes = identity_file(parent, name)?;
    credential_identity(&bytes, tool)
}

fn account_of(dir: &Path, tool: &str) -> Option<String> {
    let name = match tool {
        "claude-code" => ".claude.json",
        "codex" => "auth.json",
        _ => return None,
    };
    account_from_identity(&dir.join(name), tool)
}

fn refresh_token_identity_of(dir: &Path, tool: &str) -> Option<String> {
    let name = match tool {
        "codex" => "auth.json",
        // Claude's native identity path is a separate metadata file. Preserve
        // its existing account match rather than pretending that file carries
        // the credential selected from a file or Keychain.
        _ => return None,
    };
    let bytes = identity_file(dir, name)?;
    refresh_token_identity(&bytes, tool)
}

/// The key a claim is held under: the ACCOUNT, not the directory.
///
/// Two directories can hold one login - `doctor` reports exactly that - and
/// these tokens are single-use, so one claim per directory let a sweep spend
/// one account's token once per copy. That is the double-spend this gate
/// exists to stop, reached from the other side: the gate was made per-caller
/// once already, and the note above records why that failed.
///
/// An identity that cannot be read falls back to the path. Two unknowns are
/// not one account, and per-path is the stricter answer anyway.
fn claim_key(dir: &Path, tool: &str) -> std::path::PathBuf {
    match account_of(dir, tool) {
        Some(a) => std::path::PathBuf::from(format!("account:{a}")),
        None => std::path::PathBuf::from(format!("{tool}:{}", dir.display())),
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

/// A non-secret identifier for one exact Claude credential generation.
/// Callers that already hold the blob use this instead of racing another read
/// of the selected file or Keychain item.
pub(crate) fn claude_credential_fingerprint_from_blob(bytes: &[u8]) -> String {
    fingerprint(bytes)
}

fn source_fingerprint(dir: &Path) -> String {
    let source = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    fingerprint(source.as_os_str().as_encoded_bytes())
}

fn coordination_now_ms() -> i64 {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[derive(serde::Deserialize, serde::Serialize)]
struct DiskCompletion {
    version: u8,
    generation: String,
    #[serde(default)]
    result_generation: Option<String>,
    source: String,
    completed_at_ms: i64,
    result: DiskResult,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum DiskResult {
    Renewed,
    Busy,
    Expired,
    Refused,
    Offline,
    Superseded,
    NoAttempt,
}

impl DiskResult {
    fn from_result(result: &RefreshResult) -> Option<Self> {
        match result {
            Ok(RefreshOutcome::Renewed) => Some(Self::Renewed),
            Ok(RefreshOutcome::NativeManaged) => None,
            Err(RefreshError::Busy) => Some(Self::Busy),
            Err(RefreshError::Expired) => Some(Self::Expired),
            Err(RefreshError::Refused(_)) => Some(Self::Refused),
            Err(RefreshError::Offline(_)) => Some(Self::Offline),
            Err(RefreshError::AlreadyRefreshing) => Some(Self::Superseded),
            Err(RefreshError::NoCredential | RefreshError::InUse) => None,
        }
    }

    fn into_result(self) -> RefreshResult {
        match self {
            Self::Renewed => Ok(RefreshOutcome::Renewed),
            Self::Busy => Err(RefreshError::Busy),
            Self::Expired => Err(RefreshError::Expired),
            Self::Refused => Err(RefreshError::Refused("the login server refused it".into())),
            Self::Offline => Err(RefreshError::Offline(
                "the refresh request did not complete".into(),
            )),
            Self::Superseded | Self::NoAttempt => Err(RefreshError::AlreadyRefreshing),
        }
    }
}

struct Attempt {
    result: RefreshResult,
    result_generation: Option<String>,
    cache: bool,
    shared: bool,
}

impl Attempt {
    fn before_exchange(result: RefreshResult) -> Self {
        Self {
            result,
            result_generation: None,
            cache: false,
            shared: false,
        }
    }

    fn after_exchange(result: RefreshResult) -> Self {
        Self {
            result,
            result_generation: None,
            cache: true,
            shared: false,
        }
    }

    fn after_exchange_with_generation(result: RefreshResult, generation: Option<String>) -> Self {
        Self {
            result,
            result_generation: generation,
            cache: true,
            shared: false,
        }
    }

    fn shared(result: RefreshResult, result_generation: Option<String>) -> Self {
        Self {
            result,
            result_generation,
            cache: true,
            shared: true,
        }
    }
}

fn open_refresh_lock(paths: &Paths, key: &Path) -> std::io::Result<std::fs::File> {
    let lock_dir = paths.store_dir().join(".refresh-locks");
    std::fs::create_dir_all(&lock_dir)?;
    let lock_name = format!("{}.lock", fingerprint(key.as_os_str().as_encoded_bytes()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(lock_dir.join(lock_name))
}

fn read_disk_completion(file: &mut std::fs::File) -> Option<DiskCompletion> {
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn write_disk_completion(file: &mut std::fs::File, completion: &DiskCompletion) {
    let Ok(bytes) = serde_json::to_vec(completion) else {
        return;
    };
    if file.set_len(0).is_err() || file.rewind().is_err() {
        return;
    }
    if file.write_all(&bytes).is_ok() {
        let _ = file.sync_data();
    }
}

enum DiskDecision {
    Shared {
        result: RefreshResult,
        result_generation: Option<String>,
    },
    RetryAllowed,
}

fn disk_decision_for(
    completion: DiskCompletion,
    generation: &str,
    source: &str,
    changed_success: ChangedSuccessPolicy,
) -> Option<DiskDecision> {
    if completion.version != 1 {
        return None;
    }
    let age = coordination_now_ms().checked_sub(completion.completed_at_ms)?;
    if !(0..=BURST_SECS.saturating_mul(1000)).contains(&age) {
        return None;
    }
    if matches!(completion.result, DiskResult::NoAttempt) {
        return Some(DiskDecision::RetryAllowed);
    }
    if completion.source != source {
        return Some(DiskDecision::Shared {
            result: Err(RefreshError::AlreadyRefreshing),
            result_generation: None,
        });
    }
    if completion.generation == generation
        || completion.result_generation.as_deref() == Some(generation)
    {
        return Some(DiskDecision::Shared {
            result: completion.result.into_result(),
            result_generation: completion.result_generation,
        });
    }
    if changed_success == ChangedSuccessPolicy::Defer
        && matches!(completion.result, DiskResult::Renewed)
    {
        // This blob is not the persisted result generation. The earlier
        // exchange may still have retired the shared rotating token, so do not
        // report its success or exchange that token again.
        return Some(DiskDecision::Shared {
            result: Err(RefreshError::AlreadyRefreshing),
            result_generation: None,
        });
    }
    None
}

#[cfg(test)]
mod disk_clock_tests {
    use super::*;

    /// Keep-alive and request recovery pass the token-observation time into the
    /// refresh API. That timestamp may be fixed or captured long before a
    /// follower acquires the disk lock, so it cannot date coordination records.
    #[test]
    fn completion_freshness_ignores_the_callers_token_clock() {
        let fixed_token_clock = 1_i64;
        let completed_at_ms = coordination_now_ms();
        assert!(completed_at_ms > fixed_token_clock);
        let decision = disk_decision_for(
            DiskCompletion {
                version: 1,
                generation: "old".into(),
                result_generation: Some("new".into()),
                source: "slot".into(),
                completed_at_ms,
                result: DiskResult::Renewed,
            },
            "old",
            "slot",
            ChangedSuccessPolicy::Retry,
        );
        assert!(matches!(
            decision,
            Some(DiskDecision::Shared {
                result: Ok(RefreshOutcome::Renewed),
                ..
            })
        ));
    }
}

fn coordinate_across_processes<F>(
    paths: &Paths,
    key: &Path,
    generation: &str,
    source: &str,
    changed_success: ChangedSuccessPolicy,
    action: F,
) -> Attempt
where
    F: FnOnce() -> Attempt,
{
    let mut file = match open_refresh_lock(paths, key) {
        Ok(file) => file,
        Err(_) => {
            return Attempt::before_exchange(Err(RefreshError::Offline(
                "could not coordinate the refresh safely".into(),
            )))
        }
    };
    let started = Instant::now();
    let mut contended = false;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                contended = true;
                if started.elapsed() >= WAIT_FOR_REFRESH {
                    return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                return Attempt::before_exchange(Err(RefreshError::Offline(
                    "could not coordinate the refresh safely".into(),
                )))
            }
        }
    }

    let disk_decision = read_disk_completion(&mut file).and_then(|completion| {
        // A follower that actually waited receives the leader's full outcome.
        // A later independent command may retry a refusal, while a recorded
        // success still protects stale duplicate copies from spending the token
        // the successful exchange retired.
        let reusable_without_contention = matches!(
            completion.result,
            DiskResult::Renewed | DiskResult::Superseded
        );
        (contended
            || reusable_without_contention
            || matches!(completion.result, DiskResult::NoAttempt))
        .then(|| disk_decision_for(completion, generation, source, changed_success))
        .flatten()
    });
    if let Some(DiskDecision::Shared {
        result,
        result_generation,
    }) = disk_decision
    {
        return Attempt::shared(result, result_generation);
    }
    if contended && !matches!(disk_decision, Some(DiskDecision::RetryAllowed)) {
        // The previous holder may have exited after sending the request but
        // before recording its answer. Retrying here could spend the same
        // rotating token twice, so ambiguity is a conservative stand-down.
        return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing));
    }

    let _ = file.set_len(0);
    let attempt = action();
    if attempt.cache {
        if let Some(result) = DiskResult::from_result(&attempt.result) {
            write_disk_completion(
                &mut file,
                &DiskCompletion {
                    version: 1,
                    generation: generation.to_string(),
                    result_generation: attempt.result_generation.clone(),
                    source: source.to_string(),
                    completed_at_ms: coordination_now_ms(),
                    result,
                },
            );
        }
    } else {
        // A waiting process can distinguish a clean guard refusal from a holder
        // that may have disappeared after sending OAuth. It re-runs every guard
        // rather than turning a preflight InUse into AlreadyRefreshing.
        write_disk_completion(
            &mut file,
            &DiskCompletion {
                version: 1,
                generation: generation.to_string(),
                result_generation: None,
                source: source.to_string(),
                completed_at_ms: coordination_now_ms(),
                result: DiskResult::NoAttempt,
            },
        );
    }
    attempt
}

fn coordinate_refresh_attempt<F>(
    paths: &Paths,
    dir: &Path,
    tool: &str,
    generation: &str,
    action: F,
) -> Attempt
where
    F: FnOnce() -> Attempt,
{
    let key = claim_key(dir, tool);
    let source = source_fingerprint(dir);
    coordinate_with_key(
        paths,
        &key,
        generation,
        &source,
        ChangedSuccessPolicy::Retry,
        action,
    )
}

fn coordinate_token_refresh<F>(
    paths: &Paths,
    dir: &Path,
    tool: &str,
    token: &str,
    generation: &str,
    action: F,
) -> Attempt
where
    F: FnOnce() -> Attempt,
{
    // Callers already hold the provider-account claim. Every exchange therefore
    // takes locks in the same account-then-token order.
    let identity = refresh_token_identity_from_token(token, tool);
    let key = PathBuf::from(&identity);
    let source = source_fingerprint(dir);
    coordinate_with_key(
        paths,
        &key,
        generation,
        &source,
        ChangedSuccessPolicy::Defer,
        action,
    )
}

fn coordinate_with_key<F>(
    paths: &Paths,
    key: &Path,
    generation: &str,
    source: &str,
    changed_success: ChangedSuccessPolicy,
    action: F,
) -> Attempt
where
    F: FnOnce() -> Attempt,
{
    match gate().begin(key, generation, source, changed_success) {
        GateDecision::Shared {
            result,
            result_generation,
        } => Attempt::shared(result, result_generation),
        GateDecision::Leader => {
            let attempt = coordinate_across_processes(
                paths,
                key,
                generation,
                source,
                changed_success,
                action,
            );
            let cached = attempt
                .cache
                .then(|| (attempt.result.clone(), attempt.result_generation.clone()));
            gate().finish(key, generation, source, cached);
            attempt
        }
    }
}

fn native_managed(paths: &Paths, dir: &Path, tool: &str, now_ms: i64) -> Option<RefreshResult> {
    let login = crate::live_login::resolve(paths, dir, tool, now_ms)?;
    if login.refresh_rejected_at_ms.is_some() {
        return Some(Err(RefreshError::Expired));
    }
    Some(Ok(RefreshOutcome::NativeManaged))
}

pub fn refresh_slot(paths: &Paths, dir: &Path, now_ms: i64) -> RefreshResult {
    refresh_slot_inner(paths, dir, now_ms, None, None, None)
}

/// Renew only when both the credential source generation and every available
/// selected-account identity field still name the rejected request's owner.
pub(crate) fn refresh_slot_if_current(
    paths: &Paths,
    dir: &Path,
    now_ms: i64,
    expected_fingerprint: &str,
    expected_account_uuid: Option<&str>,
    expected_identity: Option<&crate::live_login::LoginIdentity>,
) -> RefreshResult {
    refresh_slot_inner(
        paths,
        dir,
        now_ms,
        Some(expected_fingerprint),
        expected_account_uuid,
        expected_identity,
    )
}

fn claude_identity_matches(
    dir: &Path,
    expected_account_uuid: Option<&str>,
    expected_identity: Option<&crate::live_login::LoginIdentity>,
) -> bool {
    if expected_account_uuid.is_none() && expected_identity.is_none() {
        return true;
    }
    let Some(blob) = identity_file(dir, ".claude.json") else {
        return false;
    };
    let account_uuid = serde_json::from_slice::<serde_json::Value>(&blob)
        .ok()
        .and_then(|value| {
            value["oauthAccount"]["accountUuid"]
                .as_str()
                .map(str::to_string)
        });
    if expected_account_uuid.is_some_and(|expected| account_uuid.as_deref() != Some(expected)) {
        return false;
    }
    !expected_identity.is_some_and(|expected| {
        crate::live_login::identity_from_credential(&blob, "claude-code").as_ref() != Some(expected)
    })
}

/// A live native process using the selected credential store participates in
/// Claude's two refresh locks and rereads the credential after taking them.
/// An inherited config variable alone does not prove that cooperation.
fn claude_holder_uncoordinated(
    paths: &Paths,
    dir: &Path,
    authority: &crate::claude_authority::Authority,
) -> bool {
    let held = slot_in_use(paths, dir, "claude-code");
    let expected = identity_file(dir, ".claude.json")
        .and_then(|bytes| crate::live_login::identity_from_credential(&bytes, "claude-code"));
    let selected_refresh = authority
        .read(paths)
        .ok()
        .and_then(|credential| refresh_token_identity(credential.bytes(), "claude-code"));
    let mut verified = false;
    for process in crate::proc::running_native_login_processes(paths, "claude-code") {
        let identity = identity_file(
            process.identity_path.parent().unwrap_or(Path::new("")),
            process
                .identity_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(""),
        )
        .and_then(|bytes| crate::live_login::identity_from_credential(&bytes, "claude-code"));
        let same_store = same_dir(&process.source_dir, &authority.storage_dir);
        let same_identity = expected.is_some() && identity == expected;
        let same_refresh = selected_refresh.as_ref().is_some_and(|selected| {
            crate::adapters::claude::native_credentials(
                paths,
                &process.source_dir,
                process.claude_keychain_key.as_deref(),
            )
            .as_deref()
            .and_then(|bytes| refresh_token_identity(bytes, "claude-code"))
            .as_ref()
                == Some(selected)
        });
        if !same_identity && !same_store && !same_dir(&process.source_dir, dir) && !same_refresh {
            continue;
        }
        if !same_store
            || process.claude_keychain_key != authority.securestorage_key
            || !same_identity
            || !process.supports_refresh_locks
        {
            return true;
        }
        verified = true;
    }
    held && !verified
}

fn claude_lock(
    storage_dir: &Path,
) -> Result<crate::claude_refresh_lock::NativeRefreshLock, RefreshError> {
    let started = Instant::now();
    loop {
        match crate::claude_refresh_lock::NativeRefreshLock::try_acquire(storage_dir) {
            Ok(lock) => return Ok(lock),
            Err(crate::claude_refresh_lock::LockError::Busy { .. }) => {
                if started.elapsed() >= WAIT_FOR_REFRESH {
                    return Err(RefreshError::AlreadyRefreshing);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                return Err(RefreshError::Offline(
                    "could not coordinate with Claude's refresh lock".into(),
                ))
            }
        }
    }
}

fn claude_credential_current(
    paths: &Paths,
    authority: &crate::claude_authority::Authority,
    expected: &crate::adapters::claude::SlotCredential,
) -> bool {
    authority.is_current(paths)
        && authority
            .read(paths)
            .is_ok_and(|current| current == *expected)
}

fn refresh_slot_inner(
    paths: &Paths,
    dir: &Path,
    now_ms: i64,
    expected_fingerprint: Option<&str>,
    expected_account_uuid: Option<&str>,
    expected_identity: Option<&crate::live_login::LoginIdentity>,
) -> RefreshResult {
    if !claude_identity_matches(dir, expected_account_uuid, expected_identity) {
        return Err(RefreshError::AlreadyRefreshing);
    }
    let authority = crate::claude_authority::resolve(paths, dir).map_err(|_| {
        RefreshError::Refused("Claude credential authority could not be verified".into())
    })?;
    if claude_holder_uncoordinated(paths, dir, &authority) {
        return Err(RefreshError::InUse);
    }
    let credential = authority.read(paths).map_err(|_| {
        if authority.is_bound() {
            RefreshError::Refused("Claude credential authority could not be read".into())
        } else {
            RefreshError::NoCredential
        }
    })?;
    if expected_fingerprint.is_some_and(|expected| fingerprint(credential.bytes()) != expected) {
        return Err(RefreshError::AlreadyRefreshing);
    }
    let generation = fingerprint(credential.bytes());
    let key = claim_key(dir, "claude-code");
    let source = source_fingerprint(&authority.storage_dir);

    coordinate_with_key(
        paths,
        &key,
        &generation,
        &source,
        ChangedSuccessPolicy::Retry,
        || {
            if claude_holder_uncoordinated(paths, dir, &authority) {
                return Attempt::before_exchange(Err(RefreshError::InUse));
            }
            let lock = match claude_lock(&authority.storage_dir) {
                Ok(lock) => lock,
                Err(error) => return Attempt::before_exchange(Err(error)),
            };
            // Claude itself rereads only after both native locks are held. The
            // generation observed before joining our account gate is never used
            // as the exchange input without this second read.
            if lock.ensure_owned().is_err()
                || !claude_identity_matches(dir, expected_account_uuid, expected_identity)
                || !authority.is_current(paths)
                || claude_holder_uncoordinated(paths, dir, &authority)
            {
                return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing));
            }
            let current = match authority.read(paths) {
                Ok(current) => current,
                Err(_) => return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing)),
            };
            if current != credential
                || expected_fingerprint
                    .is_some_and(|expected| fingerprint(current.bytes()) != expected)
            {
                return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing));
            }
            if refresh_token_expired(current.bytes(), now_ms) {
                return Attempt::before_exchange(Err(RefreshError::Expired));
            }
            let Some(token) = refresh_token(current.bytes()) else {
                return Attempt::before_exchange(Err(RefreshError::NoCredential));
            };
            if lock.ensure_owned().is_err() {
                return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing));
            }

            let response = post(&token);
            // A login may replace the blob while the request is in flight. Its
            // generation owns every later verdict, so neither a success nor a
            // refusal for the old token may overwrite or poison it.
            if lock.ensure_owned().is_err()
                || !claude_credential_current(paths, &authority, &current)
                || !claude_identity_matches(dir, expected_account_uuid, expected_identity)
            {
                return Attempt::after_exchange(Err(RefreshError::AlreadyRefreshing));
            }
            let (body, status) = match response {
                Ok(response) => response,
                Err(error) => return Attempt::after_exchange(Err(error)),
            };
            if status == 429 {
                return Attempt::after_exchange(Err(RefreshError::Busy));
            }
            if status == 401 || status == 400 {
                return Attempt::after_exchange(Err(RefreshError::Refused(short_reason(&body))));
            }
            if !(200..300).contains(&status) {
                return Attempt::after_exchange(Err(RefreshError::Refused(format!(
                    "HTTP {status}"
                ))));
            }
            let Some(merged) = merge_response(current.bytes(), &body, now_ms) else {
                return Attempt::after_exchange(Err(RefreshError::Refused(
                    "the server's answer had no access token".into(),
                )));
            };
            if lock.ensure_owned().is_err()
                || !claude_credential_current(paths, &authority, &current)
                || !claude_identity_matches(dir, expected_account_uuid, expected_identity)
            {
                return Attempt::after_exchange(Err(RefreshError::AlreadyRefreshing));
            }
            let result = authority
                .write(paths, current.source(), &merged)
                .map(|()| RefreshOutcome::Renewed)
                .map_err(|error| RefreshError::Refused(error.to_string()));
            let result_generation = result.is_ok().then(|| fingerprint(&merged));
            Attempt::after_exchange_with_generation(result, result_generation)
        },
    )
    .result
}

/// One short clause from an error body, for a message a person reads. Never the
/// whole body: it can be long, and it is not the user's problem to parse.
fn short_reason(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v["error_description"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["error"]["message"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "the login server refused it".into())
}

/// Is a tool currently running with this slot as its home? Checked by the
/// environment of the running processes, since that is what actually decides
/// which credential a process holds.
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

fn inside_paths(paths: &Paths, dir: &Path) -> bool {
    std::fs::canonicalize(paths.home())
        .ok()
        .zip(std::fs::canonicalize(dir).ok())
        .is_some_and(|(root, dir)| dir.starts_with(root))
}

fn credential_owner_matches(
    bytes: &[u8],
    tool: &str,
    account: Option<&str>,
    refresh_token: Option<&str>,
) -> bool {
    account.is_some_and(|candidate| credential_identity(bytes, tool).as_deref() == Some(candidate))
        || refresh_token.is_some_and(|candidate| {
            refresh_token_identity(bytes, tool).as_deref() == Some(candidate)
        })
}

fn credential_owner_matches_path(
    path: &Path,
    tool: &str,
    account: Option<&str>,
    refresh_token: Option<&str>,
) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    identity_file(parent, name)
        .is_some_and(|bytes| credential_owner_matches(&bytes, tool, account, refresh_token))
}

fn credential_owner_matches_dir(
    dir: &Path,
    tool: &str,
    account: Option<&str>,
    refresh_token: Option<&str>,
) -> bool {
    let name = match tool {
        "claude-code" => ".claude.json",
        "codex" => "auth.json",
        _ => return false,
    };
    credential_owner_matches_path(&dir.join(name), tool, account, refresh_token)
}

fn slot_in_use(paths: &Paths, dir: &Path, tool: &str) -> bool {
    let running = crate::proc::running_config_dirs(tool);
    if running.iter().any(|active| same_dir(active, dir)) {
        return true;
    }
    let candidate_account = account_of(dir, tool);
    let candidate_token = refresh_token_identity_of(dir, tool);
    if candidate_account.is_none() && candidate_token.is_none() {
        return false;
    }
    // A default Claude process reads identity from HOME/.claude.json, not from
    // ~/.claude/.claude.json. The usable-login resolver intentionally returns
    // None for expired or unverified access, but that must never erase the
    // holder guard: the process can still have the selected refresh token in
    // memory. Use the native accessor's exact identity path for this check.
    if crate::proc::running_native_login_processes(paths, tool)
        .into_iter()
        .any(|process| {
            credential_owner_matches_path(
                &process.identity_path,
                tool,
                candidate_account.as_deref(),
                candidate_token.as_deref(),
            )
        })
    {
        return true;
    }
    running.into_iter().any(|active| {
        (!paths.sandboxed() || inside_paths(paths, &active))
            && credential_owner_matches_dir(
                &active,
                tool,
                candidate_account.as_deref(),
                candidate_token.as_deref(),
            )
    })
}

fn refresh_token(blob: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(blob)
        .ok()
        .and_then(|value| {
            value["claudeAiOauth"]["refreshToken"]
                .as_str()
                .filter(|token| !token.is_empty())
                .map(str::to_string)
        })
}

#[cfg(test)]
fn write_credential_with<F, K>(
    source: crate::adapters::claude::SlotCredentialSource,
    blob: &[u8],
    write_file: F,
    write_keychain: K,
) -> anyhow::Result<()>
where
    F: FnOnce(&[u8]) -> anyhow::Result<()>,
    K: FnOnce(&[u8]) -> anyhow::Result<()>,
{
    match source {
        crate::adapters::claude::SlotCredentialSource::File => write_file(blob),
        crate::adapters::claude::SlotCredentialSource::Keychain => write_keychain(blob),
    }
}

#[cfg(test)]
mod claude_credential_source_tests {
    use super::*;
    use crate::adapters::claude::{
        choose_slot_credential, KeychainReadError, SlotCredentialSource,
    };
    use std::cell::Cell;

    fn blob(access: &str, refresh: &str, expires_at: i64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at,
                "refreshTokenExpiresAt": 9_999_999_999_999i64
            }
        }))
        .unwrap()
    }

    #[test]
    fn renewal_uses_and_writes_the_selected_keychain_generation() {
        let selected = choose_slot_credential(
            Some(blob("OLD-AT", "OLD-RT", 9_999_999_999_999)),
            Ok(blob("NEW-AT", "NEW-RT", 1_000)),
        )
        .expect("Keychain credential");
        assert_eq!(refresh_token(selected.bytes()).as_deref(), Some("NEW-RT"));

        let file_writes = Cell::new(0);
        let keychain_writes = Cell::new(0);
        write_credential_with(
            selected.source(),
            b"renewed",
            |_| {
                file_writes.set(file_writes.get() + 1);
                Ok(())
            },
            |_| {
                keychain_writes.set(keychain_writes.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(file_writes.get(), 0);
        assert_eq!(keychain_writes.get(), 1);
    }

    #[test]
    fn a_file_generation_writes_back_to_the_file() {
        let selected = choose_slot_credential(
            Some(blob("FILE-AT", "FILE-RT", 1_000)),
            Err(KeychainReadError::NotApplicable),
        )
        .expect("file credential");
        let file_writes = Cell::new(0);
        let keychain_writes = Cell::new(0);
        write_credential_with(
            selected.source(),
            b"renewed",
            |_| {
                file_writes.set(file_writes.get() + 1);
                Ok(())
            },
            |_| {
                keychain_writes.set(keychain_writes.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(file_writes.get(), 1);
        assert_eq!(keychain_writes.get(), 0);
    }

    #[test]
    fn keep_alive_uses_the_selected_keychain_expiry() {
        let now = 1_800_000_000_000;
        let selected = choose_slot_credential(
            Some(blob("OLD-AT", "OLD-RT", now + KEEP_ALIVE_WINDOW_MS * 2)),
            Ok(blob("NEW-AT", "NEW-RT", now + 1_000)),
        )
        .expect("Keychain credential");
        assert_eq!(selected.source(), SlotCredentialSource::Keychain);
        assert!(wants_keep_alive(selected.bytes(), now));
    }
}

/// POST the exchange with the token on stdin, never in argv.
fn post(refresh_token: &str) -> Result<(String, u32), RefreshError> {
    let body = request_body(refresh_token);
    // The same config shape `quota` uses, including how the status is reported:
    // run_curl reads the LAST line as the code, so decorating it broke the parse
    // and every renewal came back as HTTP 0.
    let cfg = format!(
        "url = \"{}\"\n\
         request = POST\n\
         header = \"content-type: application/json\"\n\
         header = \"Accept: application/json\"\n\
         header = \"User-Agent: swapdex\"\n\
         data = \"{}\"\n\
         silent\n\
         show-error\n\
         connect-timeout = 6\n\
         max-time = 15\n\
         write-out = \"\\n%{{http_code}}\"\n",
        token_url(),
        body.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let out = crate::quota::run_curl_cfg(&cfg).map_err(RefreshError::Offline)?;
    Ok(out)
}

/// Codex's public OAuth client id, and where its exchange happens.
///
/// Not guessed: `icoretech/codex-pooler`, an Elixir gateway that keeps a pool of
/// Codex accounts alive, carries both as constants, and the issuer matches the
/// `iss` claim on every access token in a real slot here
/// (`https://auth.openai.com`).
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Where a Codex refresh is exchanged. Redirected only under `SWAPDEX_ROOT`,
/// the same rule the Claude URL follows, so a production run can never be
/// pointed at another host holding a live refresh token.
pub fn codex_token_url() -> String {
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        if let Some(u) = std::env::var_os("SWAPDEX_CODEX_OAUTH_URL") {
            return u.to_string_lossy().into_owned();
        }
    }
    "https://auth.openai.com/oauth/token".to_string()
}

/// The form body for a Codex exchange. Form-encoded, not JSON: that is what
/// RFC 6749 specifies and what the working implementation sends.
///
/// Probed against the real endpoint with a deliberately invalid token, which
/// spends nothing: it answers a clean JSON 401 to a bare `User-Agent: swapdex`,
/// so the URL, the form shape and the client id are all accepted and no
/// browser fingerprint is needed.
pub fn codex_request_body(refresh_token: &str) -> String {
    // A JWT is unreserved characters only, but encode anyway rather than depend
    // on the shape of someone else's token.
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        enc(refresh_token),
        enc(CODEX_CLIENT_ID)
    )
}

/// Fold a Codex exchange's answer back into the slot's `auth.json`, keeping
/// every field it already had.
///
/// The rotated refresh token is written when the server sends one. Dropping it
/// would leave the slot holding a token the server has already retired - the
/// logout this project exists to prevent, arrived at from the other direction.
pub fn merge_codex_response(old: &[u8], response: &str, now_rfc3339: &str) -> Option<Vec<u8>> {
    let mut blob: serde_json::Value = serde_json::from_slice(old).ok()?;
    let r: serde_json::Value = serde_json::from_str(response).ok()?;
    let access = r["access_token"].as_str().filter(|s| !s.is_empty())?;
    let t = blob.get_mut("tokens")?.as_object_mut()?;
    t.insert("access_token".into(), access.into());
    for (from, to) in [("refresh_token", "refresh_token"), ("id_token", "id_token")] {
        if let Some(v) = r[from].as_str().filter(|s| !s.is_empty()) {
            t.insert(to.into(), v.into());
        }
    }
    blob.as_object_mut()?
        .insert("last_refresh".into(), now_rfc3339.into());
    serde_json::to_vec_pretty(&blob).ok()
}

/// `last_refresh` the way Codex writes it: RFC 3339, UTC. Only this field needs
/// it, so the civil-date arithmetic lives here rather than pulling in a clock
/// crate the rest of the tool does not use.
pub fn rfc3339_utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // days-from-civil, inverted (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Renew a Codex slot's access token in place.
///
/// The guards are the Claude path's, for the same reasons: never while Codex is
/// running in that slot (its session holds the refresh token, and retiring it
/// would break the session's own next renewal), and the claim is taken here -
/// where the token is SPENT - so two callers cannot spend it twice.
pub fn refresh_codex_slot(paths: &Paths, dir: &Path, now_ms: i64) -> RefreshResult {
    refresh_codex_slot_inner(paths, dir, now_ms, None)
}

/// Renew only when the slot still contains the credential generation and
/// provider identity that supplied the rejected request.
pub fn refresh_codex_slot_if_current(
    paths: &Paths,
    dir: &Path,
    now_ms: i64,
    expected_fingerprint: &str,
    expected_identity: Option<&crate::live_login::LoginIdentity>,
) -> RefreshResult {
    let blob = read_codex_credential(dir)?;
    let fingerprint = crate::refresh_health::codex_credential_fingerprint_from_blob(blob.expose())
        .ok_or(RefreshError::NoCredential)?;
    if fingerprint != expected_fingerprint
        || expected_identity.is_some_and(|expected| {
            crate::live_login::identity_from_credential(blob.expose(), "codex").as_ref()
                != Some(expected)
        })
    {
        return Err(RefreshError::AlreadyRefreshing);
    }
    refresh_codex_slot_inner(paths, dir, now_ms, Some(blob))
}

fn read_codex_credential(dir: &Path) -> Result<Secret, RefreshError> {
    std::fs::read(dir.join("auth.json"))
        .ok()
        .filter(|blob| !blob.is_empty())
        .map(Secret::new)
        .ok_or(RefreshError::NoCredential)
}

fn refresh_codex_slot_inner(
    paths: &Paths,
    dir: &Path,
    now_ms: i64,
    prechecked_blob: Option<Secret>,
) -> RefreshResult {
    let path = dir.join("auth.json");
    let blob = match prechecked_blob {
        Some(blob) => {
            if !std::fs::read(&path).is_ok_and(|current| current.as_slice() == blob.expose()) {
                return Err(RefreshError::AlreadyRefreshing);
            }
            if let Some(result) = native_managed(paths, dir, "codex", now_ms) {
                if !std::fs::read(&path).is_ok_and(|current| current.as_slice() == blob.expose()) {
                    return Err(RefreshError::AlreadyRefreshing);
                }
                return result;
            }
            if codex_holder_defers(paths, dir, now_ms / 1000) {
                return Err(RefreshError::InUse);
            }
            blob
        }
        None => {
            if let Some(result) = native_managed(paths, dir, "codex", now_ms) {
                return result;
            }
            if codex_holder_defers(paths, dir, now_ms / 1000) {
                return Err(RefreshError::InUse);
            }
            read_codex_credential(dir)?
        }
    };
    let token = serde_json::from_slice::<serde_json::Value>(blob.expose())
        .ok()
        .and_then(|v| {
            v["tokens"]["refresh_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .ok_or(RefreshError::NoCredential)?;
    let health_fingerprint =
        crate::refresh_health::codex_credential_fingerprint_from_blob(blob.expose())
            .ok_or(RefreshError::NoCredential)?;
    let generation = fingerprint(blob.expose());

    let attempt = coordinate_refresh_attempt(paths, dir, "codex", &generation, || {
        if !std::fs::read(&path).is_ok_and(|current| current.as_slice() == blob.expose()) {
            return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing));
        }
        if let Some(result) = native_managed(paths, dir, "codex", now_ms) {
            return Attempt::before_exchange(result);
        }
        if codex_holder_defers(paths, dir, now_ms / 1000) {
            return Attempt::before_exchange(Err(RefreshError::InUse));
        }

        coordinate_token_refresh(paths, dir, "codex", &token, &generation, || {
            // The token lock can wait behind a stale copy with a different
            // account key, so every guard is checked again after acquiring it.
            if !std::fs::read(&path).is_ok_and(|current| current.as_slice() == blob.expose()) {
                return Attempt::before_exchange(Err(RefreshError::AlreadyRefreshing));
            }
            if let Some(result) = native_managed(paths, dir, "codex", now_ms) {
                return Attempt::before_exchange(result);
            }
            if codex_holder_defers(paths, dir, now_ms / 1000) {
                return Attempt::before_exchange(Err(RefreshError::InUse));
            }

            let response = post_codex(&token);
            if !std::fs::read(&path).is_ok_and(|current| current.as_slice() == blob.expose()) {
                return Attempt::after_exchange(Err(RefreshError::AlreadyRefreshing));
            }
            let (body, status) = match response {
                Ok(response) => response,
                Err(error) => return Attempt::after_exchange(Err(error)),
            };
            if status == 429 {
                return Attempt::after_exchange(Err(RefreshError::Busy));
            }
            if matches!(status, 400 | 401 | 403) {
                let _ =
                    crate::refresh_health::record_codex_rejection(dir, &health_fingerprint, now_ms);
                return Attempt::after_exchange(Err(RefreshError::Expired));
            }
            if !(200..300).contains(&status) {
                return Attempt::after_exchange(Err(RefreshError::Refused(format!(
                    "HTTP {status}"
                ))));
            }
            let Some(merged) =
                merge_codex_response(blob.expose(), &body, &rfc3339_utc(now_ms / 1000))
            else {
                return Attempt::after_exchange(Err(RefreshError::Refused(
                    "the server's answer had no access token".into(),
                )));
            };
            if !std::fs::read(&path).is_ok_and(|current| current.as_slice() == blob.expose()) {
                return Attempt::after_exchange(Err(RefreshError::AlreadyRefreshing));
            }
            let result = crate::atomic::write_secret(&path, &merged)
                .map(|()| RefreshOutcome::Renewed)
                .map_err(|error| RefreshError::Refused(error.to_string()));
            if result.is_ok() {
                let _ = crate::refresh_health::clear_codex_rejection_before(
                    dir,
                    &health_fingerprint,
                    now_ms,
                );
            }
            let result_generation = result.is_ok().then(|| fingerprint(&merged));
            Attempt::after_exchange_with_generation(result, result_generation)
        })
    });
    if attempt.shared && matches!(&attempt.result, Ok(RefreshOutcome::Renewed)) {
        // A concurrent follower may share success for the input blob it read,
        // but only while the credential file contains the generation that the
        // leader actually persisted. If the old input was restored, reporting
        // Renewed would leave that replacement stale and could encourage reuse
        // of a rotating token the earlier exchange may have retired.
        let current_generation = std::fs::read(&path).ok().map(|bytes| fingerprint(&bytes));
        if current_generation.as_deref() != attempt.result_generation.as_deref()
            || attempt.result_generation.is_none()
        {
            return Err(RefreshError::AlreadyRefreshing);
        }
    }
    attempt.result
}

fn post_codex(refresh_token: &str) -> Result<(String, u32), RefreshError> {
    let body = codex_request_body(refresh_token);
    let cfg = format!(
        "url = \"{}\"\n\
         request = POST\n\
         header = \"content-type: application/x-www-form-urlencoded\"\n\
         header = \"Accept: application/json\"\n\
         header = \"User-Agent: swapdex\"\n\
         data = \"{}\"\n\
         silent\n\
         show-error\n\
         connect-timeout = 6\n\
         max-time = 15\n\
         write-out = \"\\n%{{http_code}}\"\n",
        codex_token_url(),
        body.replace('\\', "\\\\").replace('"', "\\\"")
    );
    crate::quota::run_curl_cfg(&cfg).map_err(RefreshError::Offline)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: &str = r#"{"claudeAiOauth":{"accessToken":"OLD-AT","refreshToken":"OLD-RT",
        "expiresAt":1000,"refreshTokenExpiresAt":9999999999999,"subscriptionType":"max"},
        "mcpOAuth":{"keep":"me"}}"#;

    #[test]
    fn a_renewal_replaces_the_refresh_token_the_server_retired() {
        let now = 1_800_000_000_000i64;
        let resp = r#"{"access_token":"NEW-AT","refresh_token":"NEW-RT","expires_in":3600}"#;
        let merged = merge_response(BLOB.as_bytes(), resp, now).expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&merged).unwrap();
        let o = &v["claudeAiOauth"];
        assert_eq!(o["accessToken"], "NEW-AT");
        assert_eq!(
            o["refreshToken"], "NEW-RT",
            "keeping the old one would leave a spent token on disk"
        );
        assert_eq!(o["expiresAt"], now + 3_600_000, "an absolute moment");
        // Everything swapdex does not understand survives.
        assert_eq!(o["subscriptionType"], "max");
        assert_eq!(v["mcpOAuth"]["keep"], "me");
    }

    // Some servers renew the access token without issuing a new refresh token.
    // Dropping the old one then would leave the account unable to renew again.
    #[test]
    fn a_response_without_a_new_refresh_token_keeps_the_old_one() {
        let resp = r#"{"access_token":"NEW-AT","expires_in":3600}"#;
        let merged = merge_response(BLOB.as_bytes(), resp, 0).expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "OLD-RT");
    }

    #[test]
    fn an_answer_with_no_access_token_is_not_merged() {
        assert!(merge_response(BLOB.as_bytes(), r#"{"error":"invalid_grant"}"#, 0).is_none());
        assert!(merge_response(BLOB.as_bytes(), "not json", 0).is_none());
        assert!(merge_response(b"not json", r#"{"access_token":"X"}"#, 0).is_none());
    }

    #[test]
    fn an_expired_refresh_token_is_named_rather_than_retried() {
        let now = 1_800_000_000_000i64;
        let blob = format!(
            r#"{{"claudeAiOauth":{{"refreshToken":"R","refreshTokenExpiresAt":{}}}}}"#,
            now - 1
        );
        assert!(refresh_token_expired(blob.as_bytes(), now));
        let msg = RefreshError::Expired.remedy("work", "claude-code");
        assert!(msg.contains("swapdex run work"), "names the way out: {msg}");
        // Unknown is not expired.
        assert!(!refresh_token_expired(br#"{"claudeAiOauth":{}}"#, now));
    }

    #[test]
    fn the_request_names_the_grant_and_the_client() {
        let b = request_body("RT-1");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["grant_type"], "refresh_token");
        assert_eq!(v["refresh_token"], "RT-1");
        assert_eq!(v["client_id"], CLIENT_ID);
    }

    // A refusal has to read as something the user can act on, never as data loss.
    #[test]
    fn a_refusal_is_reported_in_the_users_terms() {
        assert_eq!(
            short_reason(r#"{"error":"invalid_grant","error_description":"token revoked"}"#),
            "token revoked"
        );
        assert_eq!(
            short_reason(r#"{"error":{"type":"rate_limit_error","message":"Rate limited."}}"#),
            "Rate limited."
        );
        assert_eq!(
            short_reason("<html>502</html>"),
            "the login server refused it"
        );
        // Rate limiting is a wait, not a verdict: telling someone to sign in
        // again over it would cost them a login they did not need.
        let busy = RefreshError::Busy.remedy("work", "claude-code");
        assert!(busy.contains("is fine"), "{busy}");
        assert!(!busy.contains("swapdex run"), "not a sign-in: {busy}");
        let msg = RefreshError::InUse.remedy("work", "claude-code");
        assert!(msg.contains("renewal deferred"), "name the outcome: {msg}");
        assert!(
            msg.contains("refresh unverified"),
            "name uncertainty: {msg}"
        );
        assert!(!msg.contains("re-login"), "not a rejection verdict: {msg}");
    }
}

/// How long before an access token lapses a keep-alive sweep renews it.
///
/// Deliberately wide. "Is this token unusable right NOW" is a different
/// question, asked when a turn is waiting, and answered on the serving path by
/// `slot_token_expired` and `has_usable_login`. This one answers "will this
/// account still work tomorrow", and the sweep runs whether or not anybody is
/// using the account.
pub const KEEP_ALIVE_WINDOW_MS: i64 = 6 * 60 * 60 * 1000;

/// Should a keep-alive sweep renew this credential now?
///
/// An OAuth refresh token is not a key that sits still - it is exercised, and it
/// rotates. Leave an account idle long enough and its refresh token goes stale,
/// and then only a browser sign-in brings it back. Three of this machine's
/// accounts died exactly that way and stayed dead for a week.
///
/// So the sweep renews ahead of expiry rather than at it. Nothing to renew, or a
/// refresh token already gone, is not this function's problem: renewing needs a
/// refresh token, and a dead one needs a human.
pub fn wants_keep_alive(blob: &[u8], now_ms: i64) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(blob) else {
        return false;
    };
    let oauth = &v["claudeAiOauth"];
    if oauth["refreshToken"].as_str().is_none_or(str::is_empty) {
        return false;
    }
    if refresh_token_expired(blob, now_ms) {
        return false;
    }
    oauth["expiresAt"]
        .as_i64()
        .is_some_and(|exp| exp - now_ms <= KEEP_ALIVE_WINDOW_MS)
}

/// How long before a Codex access token lapses the sweep renews it.
///
/// Claude's token lives an hour, so any daily use renews it and the sweep is a
/// safety net. Codex's lives ten days and ONLY a Codex run renews it, so for an
/// account nobody has opened the sweep is the only thing standing between it and
/// a re-login. Two days is late enough that a daily sweep renews about once a
/// week rather than every pass - each renewal rotates the refresh token, and
/// rotating one more often than needed is its own risk.
pub const KEEP_ALIVE_CODEX_WINDOW_SECS: i64 = 2 * 24 * 60 * 60;

/// Should a keep-alive sweep renew this Codex credential now?
///
/// The deadline is in the access token's own `exp` claim. A credential with no
/// refresh token cannot be renewed from here, and a deadline that could not be
/// read is not an expired one - neither is grounds for spending a refresh token.
/// A holder may defer renewal only while the token it holds has a day left.
///
/// The guard exists so a session's token is not retired under it. A Codex that
/// runs refreshes its own token, so a holder that has let the token run down to
/// its final day without refreshing is not using it, and there is nothing left
/// for the guard to protect. Past this point deferring stops meaning "do not
/// retire a live token" and starts meaning "let an idle process hold this
/// account until it dies" - measured as eleven seven-day-old Codex processes
/// with zero CPU seconds between them, holding a slot 23 hours from lapsing.
pub const CODEX_HOLDER_CEILING_SECS: i64 = 24 * 60 * 60;

/// Whether a live holder still earns a deferral for this slot.
///
/// Every site that consults `slot_in_use` for Codex goes through here, so the
/// refresh path and the screens that explain it answer from one rule.
fn codex_holder_defers(paths: &Paths, dir: &Path, now_secs: i64) -> bool {
    // Ask the HOLDER's credential, not the slot's. A twin - a live process
    // holding the same account through its own home - can be fresh while the
    // slot's own token has lapsed; reading the slot there released a holder
    // that was actively using the login (tests/refresh.rs, the twin cases).
    // The idle processes this ceiling exists for hold the slot dir itself, so
    // for them the two files are one and the ceiling still bites.
    codex_holder_dirs(paths, dir).into_iter().any(|holder| {
        // Only a holder living IN the slot directory can be judged by the
        // slot's token: that is the one file it refreshes, so a full lifetime
        // without a write means it is not using the login. A holder in any
        // other home - a twin, a native process sharing the token - refreshes
        // a file this code cannot see, and a live process there is a live
        // process. The gate caught both shapes; they are honoured unconditionally.
        if !same_dir(&holder, dir) {
            return true;
        }
        let exp = std::fs::read(holder.join("auth.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v["tokens"]["access_token"]
                    .as_str()
                    .and_then(crate::proxy::codex::jwt_expiry)
            });
        // No readable expiry: keep the guard - a token that cannot be dated is
        // a token that cannot be proven abandoned.
        exp.is_none_or(|exp| exp - now_secs > CODEX_HOLDER_CEILING_SECS)
    })
}

/// Every running Codex home that holds this slot's login: the slot dir itself
/// when a process was launched there, and any other home whose credential
/// carries the same account or refresh token. Same three questions as
/// `slot_in_use`, collected instead of short-circuited.
fn codex_holder_dirs(paths: &Paths, dir: &Path) -> Vec<std::path::PathBuf> {
    let tool = "codex";
    let running = crate::proc::running_config_dirs(tool);
    let mut holders: Vec<std::path::PathBuf> = running
        .iter()
        .filter(|active| same_dir(active, dir))
        .cloned()
        .collect();
    let candidate_account = account_of(dir, tool);
    let candidate_token = refresh_token_identity_of(dir, tool);
    if candidate_account.is_none() && candidate_token.is_none() {
        return holders;
    }
    for process in crate::proc::running_native_login_processes(paths, tool) {
        if credential_owner_matches_path(
            &process.identity_path,
            tool,
            candidate_account.as_deref(),
            candidate_token.as_deref(),
        ) {
            holders.push(process.source_dir.clone());
        }
    }
    for active in running {
        if (!paths.sandboxed() || inside_paths(paths, &active))
            && credential_owner_matches_dir(
                &active,
                tool,
                candidate_account.as_deref(),
                candidate_token.as_deref(),
            )
        {
            holders.push(active);
        }
    }
    holders
}

pub fn wants_keep_alive_codex(blob: &[u8], now_secs: i64) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(blob) else {
        return false;
    };
    if v["tokens"]["refresh_token"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return false;
    }
    v["tokens"]["access_token"]
        .as_str()
        .and_then(crate::proxy::codex::jwt_expiry)
        .is_some_and(|exp| exp - now_secs <= KEEP_ALIVE_CODEX_WINDOW_SECS)
}

/// A keep-alive pass distinguishes an OAuth renewal from a native client that
/// already owns usable access. A deferred account is due, but the safety guard
/// could not prove that the live holder can serve it.
#[derive(Debug, Default)]
pub(crate) struct KeepAliveReport {
    pub(crate) renewed: Vec<String>,
    pub(crate) native_managed: Vec<String>,
    pub(crate) deferred: Vec<String>,
    pub(crate) failed: Vec<(String, RefreshError)>,
}

/// Whether current local evidence says this Codex slot is due for renewal but
/// held by a live session. This deliberately uses the same deadline predicate
/// and account-aware process guard as the refresh path itself.
pub(crate) fn codex_renewal_deferred(paths: &Paths, dir: &Path, now_secs: i64) -> bool {
    std::fs::read(dir.join("auth.json")).is_ok_and(|blob| {
        wants_keep_alive_codex(&blob, now_secs) && codex_holder_defers(paths, dir, now_secs)
    })
}

/// A Claude slot whose access is due for renewal but whose credential is held
/// by a native session. Inspect the same authoritative file or Keychain item
/// as the renewal path, then reuse its account-aware ownership guard. This
/// check only explains why renewal stood down; it never starts an exchange.
pub(crate) fn claude_renewal_deferred(paths: &Paths, dir: &Path, now_ms: i64) -> bool {
    crate::claude_authority::resolve(paths, dir).is_ok_and(|authority| {
        authority.read(paths).is_ok_and(|credential| {
            wants_keep_alive(credential.bytes(), now_ms)
                && claude_holder_uncoordinated(paths, dir, &authority)
        })
    })
}

/// The Codex half of the keep-alive sweep.
///
/// The sweep was written because an idle account's refresh token goes stale, and
/// it looked at Claude alone. A Codex account is idle BY DESIGN between runs -
/// which is the case the sweep exists for, and the one it did not cover.
pub fn keep_alive_sweep_codex(
    paths: &Paths,
    slots: &[(String, std::path::PathBuf)],
    now_ms: i64,
) -> (Vec<String>, Vec<(String, RefreshError)>) {
    let report = keep_alive_sweep_codex_report(paths, slots, now_ms);
    (report.renewed, report.failed)
}

pub(crate) fn keep_alive_sweep_codex_report(
    paths: &Paths,
    slots: &[(String, std::path::PathBuf)],
    now_ms: i64,
) -> KeepAliveReport {
    let mut report = KeepAliveReport::default();
    for (name, dir) in slots {
        let Ok(blob) = std::fs::read(dir.join("auth.json")) else {
            continue;
        };
        if !wants_keep_alive_codex(&blob, now_ms / 1000) {
            continue;
        }
        match refresh_codex_slot(paths, dir, now_ms) {
            Ok(RefreshOutcome::Renewed) => report.renewed.push(name.clone()),
            Ok(RefreshOutcome::NativeManaged) => report.native_managed.push(name.clone()),
            Err(RefreshError::InUse) => report.deferred.push(name.clone()),
            Err(e) => report.failed.push((name.clone(), e)),
        }
    }
    report
}

#[cfg(test)]
mod remedy_tool_tests {
    use super::*;

    /// A remedy that names `run` must name the tool, because `run` defaults to
    /// Claude.
    ///
    /// `Expired` is what a retired refresh token answers with - Codex returns
    /// 400/401/403 - which is the silent logout this project exists to prevent.
    /// The person reading it has a dead Codex account and was told `swapdex run
    /// <name>`; that launches Claude and registers a second account under the
    /// same name. `sign_in_remedy` in commands.rs was written for exactly this
    /// mistake, and these strings never got it.
    #[test]
    fn a_codex_remedy_names_codex() {
        for e in [RefreshError::Expired, RefreshError::NoCredential] {
            let msg = e.remedy("cxwork", "codex");
            assert!(
                msg.contains("swapdex run cxwork"),
                "the remedy stopped naming `run`, so this test measures nothing: {msg}"
            );
            assert!(
                msg.contains("swapdex run cxwork --tool codex"),
                "a Codex remedy names a command that runs Claude: {msg}"
            );
        }
    }

    /// The other direction: Claude is `run`'s default, so naming it is noise.
    #[test]
    fn a_claude_remedy_carries_no_tool_flag() {
        for e in [RefreshError::Expired, RefreshError::NoCredential] {
            let msg = e.remedy("work", "claude-code");
            assert!(
                msg.contains("swapdex run work"),
                "the remedy stopped naming `run`: {msg}"
            );
            assert!(
                !msg.contains("--tool"),
                "Claude is run's default; spelling it out is noise: {msg}"
            );
        }
    }
}

#[cfg(test)]
mod codex_keep_alive_tests {
    use super::*;

    fn jwt(exp: i64) -> String {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.sig",
            b64(br#"{"alg":"none"}"#),
            b64(format!(r#"{{"exp":{exp}}}"#).as_bytes())
        )
    }

    fn blob(exp: i64, refresh: &str) -> Vec<u8> {
        format!(
            r#"{{"tokens":{{"access_token":"{}","account_id":"a","refresh_token":"{refresh}"}}}}"#,
            jwt(exp)
        )
        .into_bytes()
    }

    /// The sweep exists because an idle account's refresh token goes stale. A
    /// Codex account is idle BY DESIGN between runs - Codex renews only when
    /// Codex runs, so the token dies exactly ten days later - which makes it the
    /// case the sweep was written for, and it was the one tool the sweep did not
    /// look at.
    #[test]
    fn a_codex_token_near_its_deadline_wants_renewing() {
        let now = 1_788_743_000i64;
        assert!(
            wants_keep_alive_codex(&blob(now + 3600, "R"), now),
            "an hour left"
        );
        assert!(
            wants_keep_alive_codex(&blob(now - 86_400, "R"), now),
            "already lapsed"
        );
        assert!(
            !wants_keep_alive_codex(&blob(now + 9 * 86_400, "R"), now),
            "nine days left is not the sweep's business yet"
        );
    }

    /// Renewing needs a refresh token, and a deadline swapdex could not read is
    /// not a deadline: neither is grounds for spending one.
    #[test]
    fn the_sweep_leaves_alone_what_it_cannot_renew() {
        let now = 1_788_743_000i64;
        assert!(
            !wants_keep_alive_codex(&blob(now + 60, ""), now),
            "no refresh token"
        );
        assert!(
            !wants_keep_alive_codex(
                br#"{"tokens":{"access_token":"not-a-jwt","refresh_token":"R"}}"#,
                now
            ),
            "an unreadable deadline is not an expired one"
        );
        assert!(!wants_keep_alive_codex(b"not json", now));
    }
}

#[cfg(test)]
mod keep_alive_tests {
    use super::*;

    fn blob(expires_in_ms: i64, refresh: &str, refresh_expiry: Option<i64>) -> Vec<u8> {
        let now = 1_700_000_000_000i64;
        let mut o = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "a",
                "refreshToken": refresh,
                "expiresAt": now + expires_in_ms,
            }
        });
        if let Some(r) = refresh_expiry {
            o["claudeAiOauth"]["refreshTokenExpiresAt"] = (now + r).into();
        }
        serde_json::to_vec(&o).unwrap()
    }
    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn an_idle_account_is_renewed_before_it_lapses() {
        let hour = 60 * 60 * 1000;
        assert!(
            wants_keep_alive(&blob(2 * hour, "r", None), NOW),
            "two hours left is inside the window"
        );
        assert!(
            !wants_keep_alive(&blob(12 * hour, "r", None), NOW),
            "half a day left needs nothing yet"
        );
    }

    /// Renewing takes a refresh token. Without one - or with one already gone -
    /// the sweep has nothing to do, and only a sign-in helps.
    #[test]
    fn there_is_nothing_to_sweep_without_a_live_refresh_token() {
        let hour = 60 * 60 * 1000;
        assert!(
            !wants_keep_alive(&blob(hour, "", None), NOW),
            "no refresh token"
        );
        assert!(
            !wants_keep_alive(&blob(hour, "r", Some(-1)), NOW),
            "the refresh token itself has expired"
        );
    }

    #[test]
    fn nonsense_is_never_swept() {
        assert!(!wants_keep_alive(b"not json", NOW));
        assert!(!wants_keep_alive(br#"{"claudeAiOauth":{}}"#, NOW));
    }
}

/// Renew every coordinated account whose token is heading for expiry. Returns the names
/// it renewed and the ones it could not, so a caller can say what happened.
///
/// Deliberately per-account and forgiving: one account's dead refresh token must
/// not stop the sweep reaching the next. Verified native holders share refresh
/// locks; unknown or conflicting holders continue to defer renewal.
pub fn keep_alive_sweep(
    paths: &Paths,
    slots: &[(String, std::path::PathBuf)],
    now_ms: i64,
) -> (Vec<String>, Vec<(String, RefreshError)>) {
    let report = keep_alive_sweep_report(paths, slots, now_ms);
    (report.renewed, report.failed)
}

pub(crate) fn keep_alive_sweep_report(
    paths: &Paths,
    slots: &[(String, std::path::PathBuf)],
    now_ms: i64,
) -> KeepAliveReport {
    let mut report = KeepAliveReport::default();
    for (name, dir) in slots {
        if crate::claude_authority::reconcile_live(paths, dir).is_err() {
            report.failed.push((
                name.clone(),
                RefreshError::Refused("Claude credential authority could not be verified".into()),
            ));
            continue;
        }
        let authority = match crate::claude_authority::resolve(paths, dir) {
            Ok(authority) => authority,
            Err(_) => {
                report.failed.push((
                    name.clone(),
                    RefreshError::Refused(
                        "Claude credential authority could not be verified".into(),
                    ),
                ));
                continue;
            }
        };
        let credential = match authority.read(paths) {
            Ok(credential) => credential,
            Err(_) if !authority.is_bound() => continue,
            Err(_) => {
                report.failed.push((
                    name.clone(),
                    RefreshError::Refused("Claude credential authority could not be read".into()),
                ));
                continue;
            }
        };
        if !wants_keep_alive(credential.bytes(), now_ms) {
            continue;
        }
        match refresh_slot(paths, dir, now_ms) {
            Ok(RefreshOutcome::Renewed) => report.renewed.push(name.clone()),
            Ok(RefreshOutcome::NativeManaged) => report.native_managed.push(name.clone()),
            Err(RefreshError::InUse) => report.deferred.push(name.clone()),
            Err(e) => report.failed.push((name.clone(), e)),
        }
    }
    report
}

#[cfg(test)]
mod point_of_effect_tests {
    use super::*;

    fn codex_identity_blob(subject: Option<&str>, workspace: &str) -> Vec<u8> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

        let id_token = subject.map(|subject| {
            format!(
                "header.{}.signature",
                URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&serde_json::json!({"sub": subject})).unwrap())
            )
        });
        serde_json::to_vec(&serde_json::json!({
            "tokens": {
                "id_token": id_token,
                "account_id": workspace
            }
        }))
        .unwrap()
    }

    #[test]
    fn codex_subject_qualifies_a_shared_workspace_identity() {
        let first = credential_identity(
            &codex_identity_blob(Some("subject-one"), "shared-workspace"),
            "codex",
        );
        let second = credential_identity(
            &codex_identity_blob(Some("subject-two"), "shared-workspace"),
            "codex",
        );

        assert_ne!(first, second, "distinct users are not one refresh owner");
    }

    #[test]
    fn opaque_codex_identity_keeps_the_workspace_fallback() {
        let missing =
            credential_identity(&codex_identity_blob(None, "fallback-workspace"), "codex");
        let malformed = serde_json::to_vec(&serde_json::json!({
            "tokens": {
                "id_token": "not-a-jwt",
                "account_id": "fallback-workspace"
            }
        }))
        .unwrap();

        assert_eq!(missing.as_deref(), Some("codex:fallback-workspace"));
        assert_eq!(
            credential_identity(&malformed, "codex").as_deref(),
            Some("codex:fallback-workspace")
        );
    }

    #[test]
    fn refresh_token_identity_is_provider_qualified_and_non_secret() {
        let raw = "ROTATING-SECRET";
        let codex = serde_json::to_vec(&serde_json::json!({
            "tokens": {"refresh_token": raw}
        }))
        .unwrap();
        let claude = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {"refreshToken": raw}
        }))
        .unwrap();

        let codex_identity = refresh_token_identity(&codex, "codex").unwrap();
        let claude_identity = refresh_token_identity(&claude, "claude-code").unwrap();
        assert_ne!(codex_identity, claude_identity);
        assert!(codex_identity.starts_with("refresh-token:codex:"));
        assert!(claude_identity.starts_with("refresh-token:claude-code:"));
        assert!(!codex_identity.contains(raw));
        assert!(!claude_identity.contains(raw));
    }

    #[test]
    fn token_alias_retries_a_changed_generation_but_shares_the_same_failure() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let slot = root.path().join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        let calls = std::cell::Cell::new(0);
        let action = || {
            calls.set(calls.get() + 1);
            Attempt::after_exchange(Err(RefreshError::Busy))
        };

        let first = coordinate_token_refresh(
            &paths,
            &slot,
            "codex",
            "GENERATION-RETRY-RT",
            "credential-generation-one",
            action,
        );
        let unchanged = coordinate_token_refresh(
            &paths,
            &slot,
            "codex",
            "GENERATION-RETRY-RT",
            "credential-generation-one",
            || {
                calls.set(calls.get() + 1);
                Attempt::after_exchange(Ok(RefreshOutcome::Renewed))
            },
        );
        let changed = coordinate_token_refresh(
            &paths,
            &slot,
            "codex",
            "GENERATION-RETRY-RT",
            "credential-generation-two",
            || {
                calls.set(calls.get() + 1);
                Attempt::after_exchange(Ok(RefreshOutcome::Renewed))
            },
        );

        assert!(matches!(first.result, Err(RefreshError::Busy)));
        assert!(matches!(unchanged.result, Err(RefreshError::Busy)));
        assert_eq!(
            changed.result,
            Ok(RefreshOutcome::Renewed),
            "a replacement credential carrying the same token inherited an old failure"
        );
        assert_eq!(calls.get(), 2, "the unchanged generation reran its action");
    }

    #[test]
    fn token_alias_reuses_only_the_exact_success_result_generation() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let slot = root.path().join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        let calls = std::cell::Cell::new(0);

        let first = coordinate_token_refresh(
            &paths,
            &slot,
            "codex",
            "SUCCESSFUL-SHARED-RT",
            "input-generation",
            || {
                calls.set(calls.get() + 1);
                Attempt::after_exchange_with_generation(
                    Ok(RefreshOutcome::Renewed),
                    Some("persisted-result-generation".into()),
                )
            },
        );
        let persisted = coordinate_token_refresh(
            &paths,
            &slot,
            "codex",
            "SUCCESSFUL-SHARED-RT",
            "persisted-result-generation",
            || {
                calls.set(calls.get() + 1);
                Attempt::after_exchange(Err(RefreshError::Busy))
            },
        );
        let unrelated = coordinate_token_refresh(
            &paths,
            &slot,
            "codex",
            "SUCCESSFUL-SHARED-RT",
            "unrelated-replacement-generation",
            || {
                calls.set(calls.get() + 1);
                Attempt::after_exchange(Ok(RefreshOutcome::Renewed))
            },
        );

        assert_eq!(first.result, Ok(RefreshOutcome::Renewed));
        assert_eq!(persisted.result, Ok(RefreshOutcome::Renewed));
        assert!(matches!(
            unrelated.result,
            Err(RefreshError::AlreadyRefreshing)
        ));
        assert_eq!(calls.get(), 1, "a shared success reran its action");
    }

    #[test]
    fn dedupe_uses_provider_qualified_account_identity() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for dir in [&a, &b] {
            std::fs::write(
                dir.join(".claude.json"),
                br#"{"oauthAccount":{"accountUuid":"same-id"}}"#,
            )
            .unwrap();
            std::fs::write(
                dir.join("auth.json"),
                br#"{"tokens":{"account_id":"same-id"}}"#,
            )
            .unwrap();
        }
        assert_eq!(
            claim_key(&a, "claude-code"),
            claim_key(&b, "claude-code"),
            "two directories holding one Claude account share a claim"
        );
        assert_ne!(
            claim_key(&a, "claude-code"),
            claim_key(&b, "codex"),
            "equal provider-local ids are not the same account"
        );
    }

    /// Standing down is not the login server refusing, and must not read as it.
    #[test]
    fn standing_down_is_its_own_answer() {
        let e = RefreshError::AlreadyRefreshing;
        let r = e.remedy("rnd", "claude-code");
        assert!(
            !r.to_lowercase().contains("sign in"),
            "nobody needs to sign in: {r}"
        );
        assert!(r.contains("rnd"), "name the account: {r}");
    }
}

// The one test inside is Linux-only, so on every other platform the module is
// empty and its `use super::*` is an unused import that `-D warnings` rejects.
// Gate the MODULE, not the test: a body that can vanish takes its imports with
// it. Only a macOS runner ever saw this, and the local gate runs on Linux.
#[cfg(all(test, target_os = "linux"))]
mod codex_in_use_tests {
    use super::*;

    /// The Codex renewal refuses a slot a live session is on.
    ///
    /// This is the guard `refresh_codex_slot` documents, and the reason the
    /// keep-alive sweep cannot log an account out: the session in that slot
    /// holds the refresh token the renewal retires, so its own next renewal
    /// would fail. The slot deliberately has no `auth.json`, so a renewal that
    /// gets past the guard stops at `NoCredential` - what this asserts against -
    /// without reaching the network.
    #[test]
    fn a_running_session_refuses_the_renewal() {
        let root = std::env::temp_dir().join(format!("swapdex_cx_ref_{}", std::process::id()));
        let slot = root.join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        let bin = root.join("codex");
        // Keep the requested comm without executing a freshly written file,
        // which can produce ETXTBSY while concurrent tests spawn children.
        let sleep = if std::path::Path::new("/bin/sleep").exists() {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        std::os::unix::fs::symlink(sleep, &bin).unwrap();

        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env("CODEX_HOME", &slot)
            .spawn()
            .unwrap();
        // Wait for the exec, not for the code under test: a process is only on
        // its slot once its environment exists.
        let environ = format!("/proc/{}/environ", child.id());
        for _ in 0..300 {
            if std::fs::read(&environ).is_ok_and(|b| !b.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let paths = Paths::rooted(&root);
        let verdict = refresh_codex_slot(&paths, &slot, 1_700_000_000_000);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            matches!(verdict, Err(RefreshError::InUse)),
            "renewed a slot a live session holds: {verdict:?}"
        );
    }
}

#[cfg(test)]
mod refresh_deadline_tests {
    use super::*;

    fn blob(refresh_exp: i64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"OLD-AT","refreshToken":"OLD-RT",
               "expiresAt":1000,"refreshTokenExpiresAt":{refresh_exp},
               "subscriptionType":"max"}}}}"#
        )
    }

    /// A renewal that mints a NEW refresh token must not keep the old one's
    /// deadline.
    ///
    /// The deadline describes the token it was issued with. Rotation retires
    /// that token, so the recorded moment now describes something that no
    /// longer exists - and `refresh_token_expired` reads it to decide whether
    /// renewing is even worth trying. Kept, it made every renewal leave the
    /// account one day closer to a sign-in swapdex would demand while holding
    /// a refresh token the server had just issued.
    #[test]
    fn a_rotated_refresh_token_does_not_inherit_the_old_deadline() {
        let now = 1_800_000_000_000i64;
        let dead_soon = now + 86_400_000; // tomorrow
        let out = merge_response(
            blob(dead_soon).as_bytes(),
            r#"{"access_token":"NEW-AT","refresh_token":"NEW-RT","expires_in":3600}"#,
            now,
        )
        .expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let o = &v["claudeAiOauth"];
        assert_eq!(o["refreshToken"], "NEW-RT", "the token rotated");
        assert!(
            !refresh_token_expired(&out, now + 2 * 86_400_000),
            "the new token inherited the retired one's deadline: {o}"
        );
    }

    /// When the server states the new token's lifetime, record it.
    #[test]
    fn a_stated_refresh_lifetime_is_recorded() {
        let now = 1_800_000_000_000i64;
        let out = merge_response(
            blob(now + 1000).as_bytes(),
            r#"{"access_token":"A","refresh_token":"R","expires_in":3600,
                "refresh_token_expires_in":604800}"#,
            now,
        )
        .expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["claudeAiOauth"]["refreshTokenExpiresAt"].as_i64(),
            Some(now + 604_800 * 1000),
            "the stated lifetime was not written"
        );
    }

    /// The sweep keeps wanting an account it has already renewed past the
    /// deadline it was signed in with.
    ///
    /// This is the whole chain the complaint was about. The sweep asks
    /// `wants_keep_alive`, which gives up when `refresh_token_expired` says the
    /// refresh token is gone - and that read the moment recorded at sign-in,
    /// which no renewal ever moved. So an account renewed every half hour for a
    /// week stopped being swept on the day its ORIGINAL token would have
    /// lapsed, while holding one the server had just issued, and the next thing
    /// the user saw was a browser sign-in.
    #[test]
    fn an_account_renewed_past_its_original_deadline_is_still_swept() {
        let now = 1_800_000_000_000i64;
        let signed_in_deadline = now + 86_400_000; // a day out, at sign-in
        let renewed = merge_response(
            blob(signed_in_deadline).as_bytes(),
            r#"{"access_token":"NEW-AT","refresh_token":"NEW-RT","expires_in":3600}"#,
            now,
        )
        .expect("merged");

        // A week later: long past the deadline the sign-in wrote down, with a
        // token that has been rotated all week.
        let later = now + 7 * 86_400_000;
        assert!(
            !refresh_token_expired(&renewed, later),
            "the sweep would call a freshly rotated token expired"
        );
        assert!(
            wants_keep_alive(&renewed, later),
            "the sweep stopped renewing an account that is still renewable"
        );
    }

    /// A renewal that does NOT rotate keeps the deadline it had.
    ///
    /// The old token is still the live one, so its deadline still describes
    /// something real. Dropping it there would throw away the one signal that
    /// says a sign-in is genuinely needed.
    #[test]
    fn a_renewal_without_rotation_keeps_the_deadline() {
        let now = 1_800_000_000_000i64;
        let deadline = now + 86_400_000;
        let out = merge_response(
            blob(deadline).as_bytes(),
            r#"{"access_token":"NEW-AT","expires_in":3600}"#,
            now,
        )
        .expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["claudeAiOauth"]["refreshTokenExpiresAt"].as_i64(),
            Some(deadline),
            "the live token's deadline was discarded"
        );
    }
}
