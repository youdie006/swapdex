use super::{Account, AuthTool, Snapshot};
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Claude;

/// Absolute path to `security`: Claude Code creates its Keychain item by
/// shelling out to `/usr/bin/security`, so the item's ACL trusts THAT binary.
/// Using the same absolute path means swapdex is the same trusted app - no
/// "allow / Always Allow" prompt - and a PATH-injected `security` can't steal
/// tokens.
const SECURITY: &str = "/usr/bin/security";

/// The prefix of the Keychain service Claude Code stores its OAuth token under.
const KEYCHAIN_PREFIX: &str = "Claude Code-credentials";

/// Whether Keychain operations may run at all. Three conditions:
/// - macOS (elsewhere Claude is file-based),
/// - NOT a unit-test build: lib tests exercise `apply` with `Paths::rooted`
///   temp dirs but no env, so on a contributor's Mac `cargo test` would write
///   sentinel tokens into the REAL Keychain (integration tests are covered
///   separately - they spawn the binary with SWAPDEX_ROOT), and
/// - NOT under SWAPDEX_ROOT. SWAPDEX_ROOT redirects every FILE path into a
///   sandbox, but the login Keychain is machine-global - without this gate a
///   `SWAPDEX_ROOT=/tmp/x swapdex use fake` test on a Mac would write the fake
///   token into the REAL Keychain item and clobber the user's actual login.
///   Under SWAPDEX_ROOT, Claude handling is file-only, like Linux.
fn keychain_enabled() -> bool {
    cfg!(target_os = "macos") && !cfg!(test) && std::env::var_os("SWAPDEX_ROOT").is_none()
}

/// The Keychain `acct` attribute Claude Code uses: `$USER`, else the OS
/// username, else a fixed fallback - matching Claude Code's own account fn.
fn keychain_account_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| "claude-code-user".into())
}

/// Pull an attribute value out of a `security` output line, e.g. the `svce`
/// (service) or `acct` printed as `    "svce"<blob>="<value>"`.
fn parse_kc_attr(line: &str, attr: &str) -> Option<String> {
    let needle = format!("\"{attr}\"");
    let rest = line.split(&needle).nth(1)?;
    let after = rest.split("=\"").nth(1)?;
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// sha256 as lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The service name computed from the env, exactly the way Claude Code derives
/// it: "Claude Code-credentials" plus, when CLAUDE_SECURESTORAGE_CONFIG_DIR /
/// CLAUDE_CONFIG_DIR is set, "-" + first 8 hex of sha256(that dir). `None`
/// when neither env var is visible to swapdex. (Claude normalizes the dir to
/// NFC before hashing; this skips that, so a non-ASCII config dir could
/// compute a different hash - discovery in `keychain_service` covers it.)
fn env_computed_service() -> Option<String> {
    match std::env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
        Ok(t) if t.is_empty() => Some(KEYCHAIN_PREFIX.to_string()),
        Ok(t) => Some(format!(
            "{KEYCHAIN_PREFIX}-{}",
            &sha256_hex(t.as_bytes())[..8]
        )),
        Err(_) => match std::env::var("CLAUDE_CONFIG_DIR") {
            Ok(d) if !d.is_empty() => Some(format!(
                "{KEYCHAIN_PREFIX}-{}",
                &sha256_hex(d.as_bytes())[..8]
            )),
            _ => None,
        },
    }
}

/// The exact Keychain service swapdex reads/writes: env-computed exact match
/// first, then discovery, then the computed name. Existence-verified.
fn keychain_service() -> Option<String> {
    if !keychain_enabled() {
        return None;
    }
    let computed = effective_computed_service();
    pick_service(
        computed.clone(),
        keychain_item_exists(&computed),
        all_claude_services(),
    )
}

/// The service name swapdex's OWN environment derives - bare when no config
/// env is set. This is exactly what a `claude` launched from the same shell
/// would use.
fn effective_computed_service() -> String {
    env_computed_service().unwrap_or_else(|| KEYCHAIN_PREFIX.to_string())
}

/// The resolution CONTRACT, mirroring Claude Code's own (Claude never scans -
/// it derives the item name purely from its env): swapdex manages the profile
/// of the environment it runs in. No env -> the DEFAULT (bare) profile; env
/// set -> that profile's suffixed item.
///
/// People run several profiles side by side via CLAUDE_CONFIG_DIR aliases
/// (`alias claude-work='CLAUDE_CONFIG_DIR=... claude'`), each with its own
/// Keychain item. The old suffix-preferring scan grabbed an ALIASED profile's
/// item while the user's plain `claude` read the bare one - switches "didn't
/// stick", and add-account deleted the wrong profile's login.
///
/// When the derived item does not exist, fall back to the scan ONLY if it is
/// unambiguous (exactly one Claude item = the alias-only setup where swapdex
/// can't see the env). With several items, guessing means reading or writing
/// some OTHER profile's login - refuse instead; `doctor` explains the state.
fn pick_service(
    computed: String,
    computed_exists: bool,
    discovered: Vec<String>,
) -> Option<String> {
    if computed_exists {
        return Some(computed);
    }
    if discovered.len() == 1 {
        return discovered.into_iter().next();
    }
    None
}

/// True if a Keychain item with this service (+ account) exists, via an
/// attribute-only lookup (no `-w`, so no ACL prompt).
fn keychain_item_exists(service: &str) -> bool {
    std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            service,
            "-a",
            &keychain_account_name(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A slot's login state, read without ever touching the secret. Doctor's rule
/// is "flag only what you can DETERMINE": a login artifact whose freshness
/// cannot be read must not masquerade as "no login yet".
#[derive(Debug, PartialEq)]
pub(crate) enum SlotLogin {
    /// No credential file and no Keychain item - the slot was never signed in.
    Absent,
    /// A login artifact exists. The value is its last-refresh signal in unix
    /// ms when determinable (`expiresAt` from `.credentials.json`, or on macOS
    /// the Keychain item's mdat, newest of the two); `None` = present but
    /// unreadable/unparseable - treated as healthy rather than guessed at.
    Present(Option<i64>),
}

/// Read a slot's [`SlotLogin`]. The Keychain lookup is attribute-only: no
/// secret read, no ACL prompt. (Service derivation hashes the raw dir string,
/// skipping Claude's NFC normalization - same documented caveat as
/// `env_computed_service`; a non-NFC path can misreport as Absent, whose
/// "run once" remedy is harmless.)
pub(crate) fn slot_login(dir: &std::path::Path) -> SlotLogin {
    let file = dir.join(".credentials.json");
    let file_present = file.exists();
    let file_ts = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["claudeAiOauth"]["expiresAt"].as_i64());
    // On macOS the Keychain is the primary store, and a leftover file from the
    // pre-Keychain era can sit ancient next to a daily-refreshed Keychain item.
    // Check BOTH and trust whichever signal is newest, or the file alone would
    // flag a healthy slot as long-idle.
    let (kc_present, kc_ts) = if keychain_enabled() {
        let service = format!(
            "{KEYCHAIN_PREFIX}-{}",
            &sha256_hex(dir.to_string_lossy().as_bytes())[..8]
        );
        if keychain_item_exists(&service) {
            (true, keychain_mdat_ms(&service))
        } else {
            (false, None)
        }
    } else {
        (false, None)
    };
    if !file_present && !kc_present {
        return SlotLogin::Absent;
    }
    SlotLogin::Present(match (file_ts, kc_ts) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    })
}

/// The modification date of a Keychain item as unix ms - the last time Claude
/// wrote (i.e. refreshed) that login. Attribute lookup only (no `-w`), so no
/// ACL prompt. `None` when the item does not exist or the date is unparseable.
fn keychain_mdat_ms(service: &str) -> Option<i64> {
    let out = std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            service,
            "-a",
            &keychain_account_name(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `security` splits attributes across stdout/stderr by version; scan both.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines().find_map(parse_mdat_ms)
}

/// Parse a `security` attribute line like
///   `    "mdat"<timedate>=0x3230... "20260722094900Z\000"`
/// into unix ms. The quoted tail is `YYYYMMDDHHMMSSZ` (UTC) plus junk.
fn parse_mdat_ms(line: &str) -> Option<i64> {
    if !line.contains("\"mdat\"<timedate>") {
        return None;
    }
    let tail = line.rsplit('"').nth(1)?; // content of the LAST quoted string
    let d: Vec<u32> = tail.chars().filter_map(|c| c.to_digit(10)).collect();
    if d.len() < 14 {
        return None;
    }
    let n = |i: usize, j: usize| -> i64 { d[i..j].iter().fold(0i64, |a, &x| a * 10 + x as i64) };
    let (y, mo, day) = (n(0, 4), n(4, 6), n(6, 8));
    let (h, mi, s) = (n(8, 10), n(10, 12), n(12, 14));
    if !(1..=12).contains(&mo) || !(1..=31).contains(&day) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some((days_from_civil(y, mo, day) * 86_400 + h * 3600 + mi * 60 + s) * 1000)
}

/// Days from 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Read a SLOT's Keychain credential blob (macOS). The service name is derived
/// from the slot's config dir the way Claude Code derives it, so this reads that
/// account's OWN item rather than the environment-derived one - which is what
/// lets proxy mode use an account other than the one this shell points at.
/// `None` off macOS, under SWAPDEX_ROOT, or when the item does not exist.
/// Why reading a slot's Keychain login failed. The distinction matters: a LOCKED
/// Keychain means the login is fine and the ENVIRONMENT is wrong (reading a
/// secret needs an unlocked login keychain, which a non-interactive ssh session
/// does not have - `errSecInteractionNotAllowed`, exit 36). Reporting that as
/// "no login" would send the user to re-sign-in an account that is already
/// signed in.
pub(crate) enum KeychainReadError {
    /// Not a Keychain environment at all (non-macOS, tests, SWAPDEX_ROOT).
    NotApplicable,
    /// The item exists but its secret cannot be read here.
    Locked,
    /// No such item, or it is empty.
    Missing,
}

/// Write a credential to the Keychain item belonging to `dir` - the same item
/// `slot_keychain_read_detail` reads, derived the same way, so a renewal lands
/// where the tool will look for it rather than beside it.
pub fn slot_keychain_write(dir: &std::path::Path, value: &[u8]) -> Result<()> {
    if !keychain_enabled() {
        anyhow::bail!("no Keychain in this environment");
    }
    keychain_write_service(&slot_service(dir), value)
}

/// The Keychain service belonging to `dir`, by Claude Code's own rule.
///
/// Claude derives the name from `CLAUDE_CONFIG_DIR`, and when that is unset -
/// which is what running plain `claude` on the default directory means - it uses
/// the bare prefix with no suffix. swapdex derived a suffix unconditionally, so
/// for `~/.claude` it read a DIFFERENT item than the tool writes: two logins for
/// one directory, and a dashboard reporting the usage of an account the user was
/// not talking to.
pub(crate) fn slot_service(dir: &std::path::Path) -> String {
    format!(
        "{KEYCHAIN_PREFIX}-{}",
        &sha256_hex(dir.to_string_lossy().as_bytes())[..8]
    )
}

pub(crate) fn slot_keychain_read_detail(
    dir: &std::path::Path,
) -> std::result::Result<Vec<u8>, KeychainReadError> {
    if !keychain_enabled() {
        return Err(KeychainReadError::NotApplicable);
    }
    let service = slot_service(dir);
    let out = std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &keychain_account_name(),
            "-w",
        ])
        .output()
        .map_err(|_| KeychainReadError::Missing)?;
    if !out.status.success() {
        // 36 = errSecInteractionNotAllowed: the item is there, the keychain is
        // locked. Anything else (44 = item not found) is a genuine absence.
        return Err(if out.status.code() == Some(36) {
            KeychainReadError::Locked
        } else {
            KeychainReadError::Missing
        });
    }
    let mut v = out.stdout;
    while v.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        v.pop();
    }
    if v.is_empty() {
        return Err(KeychainReadError::Missing);
    }
    Ok(v)
}

/// Every Keychain service name starting with the Claude prefix (attribute dump
/// only - no secret, no prompt). Feeds both the resolution fallback and the
/// `doctor` diagnostic.
fn all_claude_services() -> Vec<String> {
    let Ok(out) = std::process::Command::new(SECURITY)
        .arg("dump-keychain")
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut v: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(svc) = parse_kc_attr(line, "svce") {
            if svc.starts_with(KEYCHAIN_PREFIX) && !v.contains(&svc) {
                v.push(svc);
            }
        }
    }
    v
}

/// macOS Keychain reality for `doctor`: which Claude items exist vs the one
/// swapdex reads/writes. `None` off macOS (Claude is file-based there).
pub(crate) struct KeychainDiag {
    pub found: Vec<String>,
    pub target: Option<String>,
    /// The name swapdex's own env derives (bare when no env is set) - the
    /// item a `claude` launched from this same shell would use.
    pub computed: String,
    pub config_dir: Option<String>,
}

pub(crate) fn keychain_diagnostic() -> Option<KeychainDiag> {
    if !keychain_enabled() {
        return None;
    }
    let config_dir = std::env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("CLAUDE_CONFIG_DIR")
                .ok()
                .filter(|s| !s.is_empty())
        });
    Some(KeychainDiag {
        found: all_claude_services(),
        target: keychain_service(),
        computed: effective_computed_service(),
        config_dir,
    })
}

/// Read the Claude token JSON from the macOS Keychain (`{"claudeAiOauth":...}`).
fn keychain_read() -> Option<Vec<u8>> {
    let service = keychain_service()?;
    let out = std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &keychain_account_name(),
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut v = out.stdout;
    while v.last() == Some(&b'\n') {
        v.pop();
    }
    (!v.is_empty()).then_some(v)
}

/// The prior state of a Keychain read, kept tri-state so apply's rollback can
/// tell a genuinely-absent item (nothing to restore, delete what we create)
/// apart from a read it could not perform (must abort before mutating).
#[derive(Debug, PartialEq)]
enum KcRead {
    Present(Vec<u8>),
    Absent,
    Error,
}

/// Classify a `security find-generic-password -w` result. `security` exits 44
/// for "item not found" (a genuine absent), any other non-zero is a read error
/// we must not treat as absent. Pure so the exit-code discrimination is tested.
fn classify_kc_read(success: bool, code: Option<i32>, stdout: Vec<u8>) -> KcRead {
    if success {
        let mut v = stdout;
        while v.last() == Some(&b'\n') {
            v.pop();
        }
        return if v.is_empty() {
            KcRead::Absent
        } else {
            KcRead::Present(v)
        };
    }
    match code {
        Some(44) => KcRead::Absent, // documented "item not found"
        _ => KcRead::Error,
    }
}

/// The prior Keychain token for apply's rollback: `Ok(Some)` present,
/// `Ok(None)` genuinely absent, `Err` a read we could not classify. apply
/// aborts on `Err` BEFORE touching anything, so a rollback is always possible.
fn keychain_prior() -> Result<Option<Vec<u8>>> {
    let Some(service) = keychain_service() else {
        return Ok(None);
    };
    let out = std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &keychain_account_name(),
            "-w",
        ])
        .output()
        .context("read the current Keychain token")?;
    match classify_kc_read(out.status.success(), out.status.code(), out.stdout) {
        KcRead::Present(v) => Ok(Some(v)),
        KcRead::Absent => Ok(None),
        KcRead::Error => bail!("could not read the current Keychain token"),
    }
}

/// Write the Claude token into the Keychain, updating Claude's own item. The
/// token is passed as HEX over `security -i` stdin (via `-X`), never in argv,
/// so it can't be read from `ps`.
fn keychain_write(value: &[u8]) -> Result<()> {
    // Write ONLY to the item this environment DERIVES - a `claude` launched
    // from the same env reads exactly that one. Never write to a DISCOVERED
    // item: with no env and a single existing alias, keychain_service()'s scan
    // fallback would return that alias, so a plain `swapdex use` would overwrite
    // a DIFFERENT profile's login. The env-derived name is exact and, when the
    // item does not exist yet, is the correct slot to create.
    keychain_write_service(&effective_computed_service(), value)
}

/// Write `value` to an EXACT Keychain `service`. apply-WAL recovery restores the
/// exact service it recorded before the interrupted write, rather than
/// re-deriving it.
fn keychain_write_service(service: &str, value: &[u8]) -> Result<()> {
    use std::io::Write;
    // Defense in depth: apply() already skips the Keychain when disabled, but
    // a future caller must never be able to write a sandboxed test token into
    // the real Keychain.
    if !keychain_enabled() {
        return Ok(());
    }
    let acct = keychain_account_name();
    let hex: String = value.iter().map(|b| format!("{b:02x}")).collect();
    let cmd = format!("add-generic-password -U -a \"{acct}\" -s \"{service}\" -X {hex}\n");
    let mut child = std::process::Command::new(SECURITY)
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("run `/usr/bin/security -i`")?;
    child
        .stdin
        .as_mut()
        .context("security stdin")?
        .write_all(cmd.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "Keychain write failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Remove Claude's Keychain item so `claude` prompts a FRESH sign-in during
/// the add-a-new-account flow. Deletes ONLY the item this environment resolves
/// to - other CLAUDE_CONFIG_DIR profiles' items are other logins, and the old
/// "also clear the bare name" extra could kill a LIVE default profile. No-op
/// off macOS or when nothing resolves.
pub(crate) fn keychain_delete() {
    // Delete the env-DERIVED item only, never a discovered one: sign-out during
    // add-account must not remove some OTHER CLAUDE_CONFIG_DIR profile's login
    // just because it happens to be the single item the scan found.
    keychain_delete_service(&effective_computed_service());
}

/// Delete an EXACT Keychain `service`. apply-WAL recovery deletes the exact item
/// it created (the recorded service) when rolling back a created-item apply.
fn keychain_delete_service(service: &str) {
    if !keychain_enabled() {
        return;
    }
    let acct = keychain_account_name();
    let _ = std::process::Command::new(SECURITY)
        .args(["delete-generic-password", "-s", service, "-a", &acct])
        .output();
}

/// A roll-back journal for the multi-resource `apply` (#1). apply mutates three
/// resources in order - the credential FILE, the macOS Keychain, and
/// `.claude.json`'s oauthAccount. A SIGKILL / power loss BETWEEN them would
/// leave A's token with B's identity, which a later `use` would silently apply.
/// So apply writes+fsyncs this journal (the PRIOR state of each resource) BEFORE
/// the first mutation, and removes it once the state is consistent again (all
/// three written, or all three rolled back). On the next apply/capture, a
/// surviving journal means the run was interrupted: `recover_interrupted_apply`
/// rolls the login back to that prior state. Roll-back, not roll-forward: the
/// interrupted switch is simply undone and the user re-runs it.
#[derive(serde::Serialize, serde::Deserialize)]
struct ApplyWal {
    cred_path: String,
    /// prior credential-file bytes as hex; None = the file was absent.
    cred_prior_hex: Option<String>,
    cfg_path: String,
    /// prior `oauthAccount` value; None = the key was absent.
    cfg_oauth_prior: Option<Value>,
    /// the exact Keychain service (macOS only); None off macOS.
    kc_service: Option<String>,
    /// prior Keychain token as hex; None = the item was absent (created by apply).
    kc_prior_hex: Option<String>,
}

pub(crate) fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn apply_wal_path(paths: &Paths) -> std::path::PathBuf {
    paths.store_dir().join("apply-claude.wal")
}

fn write_apply_wal(paths: &Paths, wal: &ApplyWal) -> Result<()> {
    let p = apply_wal_path(paths);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let bytes = serde_json::to_vec(wal).context("serialize apply WAL")?;
    // 0600 + atomic + fsync: the journal must be durable before the first write.
    crate::atomic::write_secret(&p, &bytes)
}

fn remove_apply_wal(paths: &Paths) {
    let _ = std::fs::remove_file(apply_wal_path(paths));
}

/// Roll the Claude login back to the pre-switch state a surviving WAL records,
/// so a crash mid-`apply` never leaves a mixed token/identity. Best-effort per
/// resource; the WAL is removed only once every restore that applies has
/// succeeded, so an unrecoverable slice is retried on the next call rather than
/// silently left mixed. No-op when there is no WAL.
pub(crate) fn recover_interrupted_apply(paths: &Paths) {
    let p = apply_wal_path(paths);
    let Ok(bytes) = std::fs::read(&p) else {
        return;
    };
    let Ok(wal) = serde_json::from_slice::<ApplyWal>(&bytes) else {
        return; // unparseable: leave for inspection, never guess
    };
    let mut ok = true;

    // .claude.json oauthAccount -> prior (or remove the key if it was absent).
    let cfg_path = std::path::PathBuf::from(&wal.cfg_path);
    if cfg_path.exists() {
        if let Ok(cur) = crate::atomic::read_regular(&cfg_path) {
            if let Ok(mut v) = serde_json::from_slice::<Value>(&cur) {
                if let Some(obj) = v.as_object_mut() {
                    match &wal.cfg_oauth_prior {
                        Some(prior) => {
                            obj.insert("oauthAccount".into(), prior.clone());
                        }
                        None => {
                            obj.remove("oauthAccount");
                        }
                    }
                    match serde_json::to_vec(&v) {
                        Ok(nb) => ok &= crate::atomic::write_secret(&cfg_path, &nb).is_ok(),
                        Err(_) => ok = false,
                    }
                }
            }
        }
    }

    // credential file -> prior (or delete if it was absent).
    let cred_path = std::path::PathBuf::from(&wal.cred_path);
    match &wal.cred_prior_hex {
        Some(hex) => match from_hex(hex) {
            Some(b) => ok &= crate::atomic::write_secret(&cred_path, &b).is_ok(),
            None => ok = false,
        },
        None => {
            let _ = std::fs::remove_file(&cred_path);
        }
    }

    // macOS Keychain -> prior (or delete the item apply created). Off macOS the
    // service is None and this is skipped.
    if let Some(service) = &wal.kc_service {
        match &wal.kc_prior_hex {
            Some(hex) => match from_hex(hex) {
                Some(b) => ok &= keychain_write_service(service, &b).is_ok(),
                None => ok = false,
            },
            None => keychain_delete_service(service),
        }
    }

    if ok {
        remove_apply_wal(paths);
    }
}

/// The Claude token JSON from wherever it lives: the file when present,
/// otherwise the macOS Keychain.
fn cred_read(paths: &Paths) -> Option<Vec<u8>> {
    // On macOS the Keychain is AUTHORITATIVE: Claude reads its token there and
    // silently refreshes it (rotating the refresh token) without rewriting the
    // credential FILE that swapdex leaves behind. Reading the file first would
    // hand back a stale, possibly-revoked token - and the switch-away writeback
    // would then persist that stale token into the profile, losing the live
    // login. So prefer the Keychain when it is in play; the file is only the
    // fallback (a Keychain locked/absent, or an install that still uses it).
    if keychain_enabled() {
        return keychain_read().or_else(|| {
            let f = paths.claude_credentials();
            f.exists()
                .then(|| crate::atomic::read_regular(&f).ok())
                .flatten()
        });
    }
    let f = paths.claude_credentials();
    if f.exists() {
        crate::atomic::read_regular(&f).ok()
    } else {
        keychain_read()
    }
}

/// The LIVE Claude credential JSON (file or Keychain) - the active account's
/// token, kept fresh by Claude Code. Used only by `swapdex quota` to read the
/// active account's remaining quota; never leaves the machine except as that
/// account's own bearer token to Anthropic's usage endpoint.
pub(crate) fn live_credentials(paths: &Paths) -> Option<Vec<u8>> {
    cred_read(paths)
}

/// True if a Claude login exists at all (file or Keychain).
fn cred_present(paths: &Paths) -> bool {
    paths.claude_credentials().exists() || keychain_read().is_some()
}

impl AuthTool for Claude {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn present(&self, paths: &Paths) -> bool {
        cred_present(paths)
    }

    fn capture(&self, paths: &Paths) -> Result<Snapshot> {
        // Heal a crashed apply before reading, so we never capture a mixed
        // (A-token + B-identity) live state into a profile.
        recover_interrupted_apply(paths);
        let Some(cred_bytes) = cred_read(paths) else {
            bail!("not logged in to Claude Code");
        };
        serde_json::from_slice::<Value>(&cred_bytes)
            .context("the Claude credential is not valid JSON")?;
        // Extract ONLY the oauthAccount block from .claude.json, never the file.
        // Right after a fresh CLI login .claude.json may not exist yet; treat a
        // missing config as an absent oauthAccount rather than failing capture.
        let cfg_path = paths.claude_config_json();
        let cfg: Value = if cfg_path.exists() {
            serde_json::from_slice(&crate::atomic::read_regular(&cfg_path)?).context(
                "your LIVE ~/.claude.json is corrupt (not the profile snapshot) - \
                     repair or remove that file, then retry; removing loses local \
                     settings like project trust",
            )?
        } else {
            Value::Null
        };
        let oauth = cfg.get("oauthAccount").cloned().unwrap_or(Value::Null);
        let oauth_bytes = serde_json::to_vec(&oauth)?;
        Ok(Snapshot {
            tool: "claude-code",
            blobs: vec![
                ("credentials".into(), Secret::new(cred_bytes)),
                ("oauth_account".into(), Secret::new(oauth_bytes)),
            ],
        })
    }

    fn apply(&self, paths: &Paths, snap: &Snapshot) -> Result<()> {
        // Heal a previously-interrupted apply (roll it back to its pre-switch
        // state) before starting, so we never build on a crashed, mixed login.
        recover_interrupted_apply(paths);
        let cred = snap
            .part("credentials")
            .context("snapshot missing credentials")?;
        let oauth = snap
            .part("oauth_account")
            .context("snapshot missing oauth_account")?;
        // Validate BOTH blobs before touching any live file, so a corrupt
        // snapshot can never brick the login (never write unvalidated bytes).
        serde_json::from_slice::<Value>(cred.expose())
            .context("saved credentials are not valid JSON; refusing to apply")?;
        let oauth_val: Value = serde_json::from_slice(oauth.expose())
            .context("saved oauthAccount is not valid JSON; refusing to apply")?;
        // Build the new .claude.json bytes (read-modify-write: replace ONLY the
        // oauthAccount key, preserve projects/mcpServers/theme/... - the A1
        // guarantee) BEFORE writing anything, so both writes are prepared first.
        let cfg_path = paths.claude_config_json();
        let mut cfg: Value = if cfg_path.exists() {
            serde_json::from_slice(&crate::atomic::read_regular(&cfg_path)?).context(
                "your LIVE ~/.claude.json is corrupt (not the profile snapshot) - \
                     repair or remove that file, then retry; removing loses local \
                     settings like project trust",
            )?
        } else {
            Value::Object(Default::default())
        };
        // The prior oauthAccount, captured before we overwrite it - the WAL's
        // roll-back target for the config resource.
        let cfg_oauth_prior = cfg.get("oauthAccount").cloned();
        match cfg.as_object_mut() {
            Some(obj) => {
                obj.insert("oauthAccount".into(), oauth_val);
            }
            None => bail!(".claude.json is not a JSON object"),
        }
        let new_cfg = serde_json::to_vec(&cfg)?;
        // Three writes, both-or-neither: the credential FILE, the macOS
        // Keychain (Claude Code reads its token from there), and the config
        // file's oauthAccount. Snapshot the previous state of each so any
        // failure rolls ALL of them back - the login is never half-swapped.
        let cred_path = paths.claude_credentials();
        // keychain_enabled, not bare cfg!(macos): under SWAPDEX_ROOT the
        // Keychain must stay untouched (file-only, like Linux), or a sandboxed
        // test switch would overwrite the REAL login token.
        let macos = keychain_enabled();
        let prev_file = if cred_path.exists() {
            crate::atomic::read_regular(&cred_path).ok()
        } else {
            None
        };
        // Read the prior Keychain token BEFORE any mutation, tri-state: a read
        // we cannot perform aborts HERE (a later rollback would be impossible),
        // never proceeds as if the item were absent.
        let prev_kc = if macos {
            match keychain_prior() {
                Ok(v) => v,
                Err(e) => {
                    return Err(e.context(
                        "apply aborted before any change - could not read the current \
                         Keychain token, so a failed switch could not be rolled back",
                    ))
                }
            }
        } else {
            None
        };

        // Journal every resource's PRIOR state and fsync it BEFORE the first
        // mutation, so a crash mid-apply is rolled back on the next apply/capture
        // (recover_interrupted_apply). Removed once the state is consistent again.
        let wal = ApplyWal {
            cred_path: cred_path.to_string_lossy().into_owned(),
            cred_prior_hex: prev_file.as_deref().map(to_hex),
            cfg_path: cfg_path.to_string_lossy().into_owned(),
            cfg_oauth_prior,
            kc_service: if macos {
                Some(effective_computed_service())
            } else {
                None
            },
            kc_prior_hex: prev_kc.as_deref().map(to_hex),
        };
        write_apply_wal(paths, &wal)?;

        let restore_file = |prev: &Option<Vec<u8>>| match prev {
            Some(p) => crate::atomic::write_secret(&cred_path, p).is_ok(),
            None => std::fs::remove_file(&cred_path).is_ok() || !cred_path.exists(),
        };

        // 1) credential file (keeps Claude working on Linux, and on macOS
        //    installs that also read the file).
        crate::atomic::write_secret(&cred_path, cred.expose())?;
        // 2) macOS Keychain - the source of truth for Claude on macOS.
        if macos {
            if let Err(e) = keychain_write(cred.expose()) {
                if restore_file(&prev_file) {
                    remove_apply_wal(paths); // rolled back to a consistent state
                }
                return Err(e.context("apply aborted; credential file rolled back"));
            }
        }
        // 3) config oauthAccount.
        if let Err(e) = crate::atomic::write_secret(&cfg_path, &new_cfg) {
            let f_ok = restore_file(&prev_file);
            let k_ok = if macos {
                match &prev_kc {
                    Some(p) => keychain_write(p).is_ok(),
                    None => {
                        // The item was CREATED by this apply (no prior token).
                        // Leaving it would strand A's token in the Keychain while
                        // the file+config roll back to B - a token/identity
                        // mismatch a later `use` would silently apply. Delete
                        // exactly what we created.
                        keychain_delete();
                        true
                    }
                }
            } else {
                true
            };
            let msg = if f_ok && k_ok {
                // Rolled back to a consistent (pre-switch) state - retire the WAL.
                remove_apply_wal(paths);
                "apply aborted; the credential change was rolled back"
            } else {
                // Left mixed: keep the WAL so recover_interrupted_apply retries.
                "apply aborted and the rollback FAILED - the login may be half-swapped; \
                 run `swapdex restore --tool claude` once the underlying problem is fixed"
            };
            return Err(e.context(msg));
        }
        // All three resources written: the login is consistent - retire the WAL.
        remove_apply_wal(paths);
        Ok(())
    }

    fn identity(&self, paths: &Paths) -> Result<Option<Account>> {
        // The token comes from the file or the macOS Keychain; the identity
        // (email/uuid) is always in .claude.json.
        let Some(cred_bytes) = cred_read(paths) else {
            return Ok(None);
        };
        let creds: Value = serde_json::from_slice(&cred_bytes)
            .context("the Claude credential is not valid JSON")?;
        let expires_at = creds["claudeAiOauth"]["expiresAt"].as_i64();
        let tier = creds["claudeAiOauth"]["subscriptionType"]
            .as_str()
            .map(|s| s.to_string());
        let cfg_path = paths.claude_config_json();
        let cfg: Value = if cfg_path.exists() {
            crate::atomic::read_regular(&cfg_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let oauth = &cfg["oauthAccount"];
        Ok(Some(Account {
            tool: "claude-code",
            account_id: oauth["accountUuid"].as_str().unwrap_or("").to_string(),
            display: oauth["displayName"]
                .as_str()
                .unwrap_or("Claude account")
                .to_string(),
            email: oauth["emailAddress"].as_str().map(|s| s.to_string()),
            tier,
            expires_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::slot_service;

    // Every slot's item is named from its directory, including `~/.claude` when
    // that directory is a registered account - which is what a slot IS, and how
    // Claude names it when launched with CLAUDE_CONFIG_DIR set to it.
    //
    // An unsuffixed item can also exist for `~/.claude`, left by a launch with
    // no CLAUDE_CONFIG_DIR at all. It is a DIFFERENT login, and on the machine
    // this was checked on it was a dead one from a previous account - so reading
    // it in preference would have replaced a live credential with a corpse.
    #[test]
    fn a_slot_is_named_from_its_own_directory() {
        let home = std::path::Path::new("/Users/me/.claude");
        let got = slot_service(home);
        assert!(got.starts_with("Claude Code-credentials-"), "{got}");
        assert_eq!(got.len(), "Claude Code-credentials-".len() + 8);
        // Two directories never collide.
        assert_ne!(
            got,
            slot_service(std::path::Path::new("/Users/me/.claude-work"))
        );
    }

    use super::*;
    use crate::paths::Paths;
    use serde_json::json;

    // The Keychain `mdat` attribute line parses to unix ms; anything else
    // parses to nothing. The `\000` junk after the date is LITERAL backslash
    // digits in `security` output and must not corrupt the date.
    #[test]
    fn mdat_line_parses_to_unix_ms() {
        let line = "    \"mdat\"<timedate>=0x32303234303130313030303030305A00  \
                    \"20240101000000Z\\000\"";
        assert_eq!(parse_mdat_ms(line), Some(1_704_067_200_000));
        // Epoch itself.
        let epoch = "\"mdat\"<timedate>=0x00  \"19700101000000Z\\000\"";
        assert_eq!(parse_mdat_ms(epoch), Some(0));
        // Not an mdat line / malformed date -> None.
        assert_eq!(
            parse_mdat_ms("\"cdat\"<timedate>=0x00 \"20240101000000Z\""),
            None
        );
        assert_eq!(parse_mdat_ms("\"mdat\"<timedate>=0x00 \"2024\""), None);
        assert_eq!(
            parse_mdat_ms("\"mdat\"<timedate>=0x00 \"20241301000000Z\""),
            None,
            "month 13 rejected"
        );
    }

    // The civil-date helper against known anchors (leap handling included).
    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 1, 1), 19723);
        assert_eq!(days_from_civil(2024, 3, 1), 19783, "2024 is a leap year");
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    // Slot login tri-state (keychain is disabled in unit tests, so file-only):
    // no file = Absent; a parseable file = Present with its expiresAt; a file
    // that EXISTS but cannot be parsed = Present(None), never Absent - doctor
    // must not call a real login "no login yet".
    #[test]
    fn slot_login_distinguishes_absent_present_and_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(slot_login(dir.path()), SlotLogin::Absent, "no login yet");
        std::fs::write(
            dir.path().join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"AT","expiresAt":1704067200000}}"#,
        )
        .unwrap();
        assert_eq!(
            slot_login(dir.path()),
            SlotLogin::Present(Some(1_704_067_200_000))
        );
        std::fs::write(dir.path().join(".credentials.json"), b"not json").unwrap();
        assert_eq!(slot_login(dir.path()), SlotLogin::Present(None));
    }

    // The resolution contract: manage the profile of the environment swapdex
    // runs in; fall back to the scan only when it is unambiguous.
    #[test]
    fn pick_service_prefers_the_env_derived_item() {
        // The derived item exists: use it, even when aliased siblings exist.
        // (The old suffix-preferring scan would have grabbed a sibling here -
        // the "switch didn't stick / wrong profile wiped" root cause.)
        let siblings = vec![
            "Claude Code-credentials-5953ba74".to_string(),
            "Claude Code-credentials-feeb5ea6".to_string(),
        ];
        assert_eq!(
            pick_service("Claude Code-credentials".into(), true, siblings),
            Some("Claude Code-credentials".to_string())
        );
    }

    #[test]
    fn pick_service_falls_back_only_when_unambiguous() {
        // Derived item missing + exactly one login: manage that one
        // (alias-only setup where swapdex can't see the env).
        assert_eq!(
            pick_service(
                "Claude Code-credentials".into(),
                false,
                vec!["Claude Code-credentials-5953ba74".to_string()],
            ),
            Some("Claude Code-credentials-5953ba74".to_string())
        );
        // Derived item missing + several logins: refuse to guess - reading or
        // writing would hit some OTHER profile's login.
        assert_eq!(
            pick_service(
                "Claude Code-credentials".into(),
                false,
                vec![
                    "Claude Code-credentials-5953ba74".to_string(),
                    "Claude Code-credentials-feeb5ea6".to_string(),
                ],
            ),
            None
        );
        // Nothing anywhere: nothing to manage.
        assert_eq!(
            pick_service("Claude Code-credentials".into(), false, vec![]),
            None
        );
    }

    fn seed_claude(p: &Paths, acct: &str, email: &str) {
        std::fs::create_dir_all(p.claude_credentials().parent().unwrap()).unwrap();
        std::fs::write(
            p.claude_credentials(),
            serde_json::to_vec(&json!({"claudeAiOauth": {
                "accessToken": "AT-SENTINEL", "refreshToken": "RT-SENTINEL",
                "expiresAt": 9999999999999i64, "scopes": ["x"],
                "subscriptionType": "max", "rateLimitTier": "default"}}))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            p.claude_config_json(),
            serde_json::to_vec(&json!({
                "projects": {"/home/x/proj": {"trust": true}},
                "mcpServers": {"prodex": {"command": "prodex"}},
                "theme": "dark",
                "oauthAccount": {"accountUuid": acct, "emailAddress": email,
                                 "displayName": "Work", "userRateLimitTier": "max"}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(&super::sha256_hex(b"abc")[..8], "ba7816bf");
    }

    #[test]
    fn keychain_attr_parser_reads_svce_and_acct() {
        assert_eq!(
            super::parse_kc_attr("    \"acct\"<blob>=\"bsgong\"", "acct").as_deref(),
            Some("bsgong")
        );
        assert_eq!(
            super::parse_kc_attr(
                "    \"svce\"<blob>=\"Claude Code-credentials-5953ba74\"",
                "svce"
            )
            .as_deref(),
            Some("Claude Code-credentials-5953ba74")
        );
        assert_eq!(super::parse_kc_attr("no attr here", "acct"), None);
    }

    // #3: apply's rollback must tell a genuinely-absent Keychain item (delete
    // what we created) from a read it could not perform (abort before mutating).
    // The real Keychain is macOS-only and off under SWAPDEX_ROOT, so the
    // exit-code discrimination that drives that decision is tested here.
    #[test]
    fn classify_kc_read_distinguishes_absent_from_error() {
        use super::{classify_kc_read, KcRead};
        // exit 44 is `security`'s documented "item not found" - genuinely absent.
        assert_eq!(classify_kc_read(false, Some(44), vec![]), KcRead::Absent);
        // any other non-zero is a read we could NOT perform - never "absent".
        assert_eq!(classify_kc_read(false, Some(1), vec![]), KcRead::Error);
        assert_eq!(classify_kc_read(false, None, vec![]), KcRead::Error);
        // success with bytes = present (trailing newline stripped); empty = absent.
        assert_eq!(
            classify_kc_read(true, Some(0), b"tok\n".to_vec()),
            KcRead::Present(b"tok".to_vec())
        );
        assert_eq!(classify_kc_read(true, Some(0), vec![]), KcRead::Absent);
    }

    // C1: if the .claude.json write fails after credentials are written, the
    // credentials must roll back so the login is never left half-swapped.
    #[test]
    fn apply_rolls_back_credentials_when_config_write_fails() {
        let a = tempfile::tempdir().unwrap();
        let pa = Paths::rooted(a.path());
        seed_claude(&pa, "uuid-A", "a@x.com");
        let snap = Claude.capture(&pa).unwrap();

        let b = tempfile::tempdir().unwrap();
        let pb = Paths::rooted(b.path());
        seed_claude(&pb, "uuid-B", "b@y.com");
        let orig_creds = std::fs::read(pb.claude_credentials()).unwrap();

        // Block the config write so it fails AFTER the credentials have already
        // been swapped. Planting a directory at the atomic temp path used to
        // work, but that path now carries a pid and a counter so two writers
        // cannot collide - and a test that knows the temp name is testing the
        // name. Make the DESTINATION unwritable instead, which is what the
        // rollback is actually about.
        let cfg = pb.claude_config_json();
        std::fs::remove_file(&cfg).ok();
        std::fs::create_dir(&cfg).unwrap();

        assert!(Claude.apply(&pb, &snap).is_err(), "config write must fail");
        assert_eq!(
            std::fs::read(pb.claude_credentials()).unwrap(),
            orig_creds,
            "credentials must roll back to B - never half-swapped"
        );
    }

    // #1: a clean apply retires its WAL (no leftover journal to recover from).
    #[test]
    fn apply_leaves_no_wal_on_success() {
        let a = tempfile::tempdir().unwrap();
        let pa = Paths::rooted(a.path());
        seed_claude(&pa, "uuid-A", "a@x.com");
        let snap = Claude.capture(&pa).unwrap();
        let b = tempfile::tempdir().unwrap();
        let pb = Paths::rooted(b.path());
        seed_claude(&pb, "uuid-B", "b@y.com");
        Claude.apply(&pb, &snap).unwrap();
        assert!(
            !apply_wal_path(&pb).exists(),
            "WAL is retired once the apply is consistent"
        );
    }

    // #1: a crash mid-apply (WAL survives beside a half-written login) is rolled
    // back to the pre-switch state - never a mixed A-token + B-identity. The
    // macOS Keychain slice is exercised only on macOS; here (file + config) is
    // the path that protects Linux/WSL.
    #[test]
    fn recover_rolls_back_a_crashed_apply_to_prior() {
        let b = tempfile::tempdir().unwrap();
        let pb = Paths::rooted(b.path());
        seed_claude(&pb, "uuid-B", "b@y.com");
        let cred_path = pb.claude_credentials();
        let cfg_path = pb.claude_config_json();
        let prior_cred = std::fs::read(&cred_path).unwrap();
        let prior_oauth: Value =
            serde_json::from_slice::<Value>(&std::fs::read(&cfg_path).unwrap())
                .unwrap()
                .get("oauthAccount")
                .cloned()
                .unwrap();
        // Journal B's prior state, as apply does before its first mutation.
        let wal = ApplyWal {
            cred_path: cred_path.to_string_lossy().into_owned(),
            cred_prior_hex: Some(to_hex(&prior_cred)),
            cfg_path: cfg_path.to_string_lossy().into_owned(),
            cfg_oauth_prior: Some(prior_oauth.clone()),
            kc_service: None, // Linux/WSL: no Keychain
            kc_prior_hex: None,
        };
        write_apply_wal(&pb, &wal).unwrap();
        // Simulate the crash: A's token + A's identity were written, WAL survived.
        std::fs::write(&cred_path, br#"{"claudeAiOauth":{"accessToken":"AT-A"}}"#).unwrap();
        std::fs::write(
            &cfg_path,
            serde_json::to_vec(&json!({"oauthAccount": {"accountUuid": "uuid-A"}})).unwrap(),
        )
        .unwrap();

        recover_interrupted_apply(&pb);

        assert_eq!(
            std::fs::read(&cred_path).unwrap(),
            prior_cred,
            "credential file rolled back to B"
        );
        let after: Value = serde_json::from_slice(&std::fs::read(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            after["oauthAccount"], prior_oauth,
            "oauthAccount rolled back to B"
        );
        assert!(
            !apply_wal_path(&pb).exists(),
            "WAL removed after a successful recovery"
        );
    }

    #[test]
    fn apply_swaps_only_oauthaccount_and_preserves_siblings() {
        let a = tempfile::tempdir().unwrap();
        let pa = Paths::rooted(a.path());
        seed_claude(&pa, "uuid-A", "a@x.com");
        let snap = Claude.capture(&pa).unwrap();

        // A DIFFERENT existing config on machine B with its own projects/mcp.
        let b = tempfile::tempdir().unwrap();
        let pb = Paths::rooted(b.path());
        seed_claude(&pb, "uuid-B", "b@y.com");
        std::fs::write(
            pb.claude_config_json(),
            serde_json::to_vec(&json!({
                "projects": {"/keep/me": {"trust": true}},
                "mcpServers": {"sessionwiki": {"command": "sessionwiki"}},
                "theme": "light",
                "oauthAccount": {"accountUuid": "uuid-B", "emailAddress": "b@y.com"}
            }))
            .unwrap(),
        )
        .unwrap();

        Claude.apply(&pb, &snap).unwrap();

        let after: Value =
            serde_json::from_slice(&std::fs::read(pb.claude_config_json()).unwrap()).unwrap();
        // oauthAccount switched to A...
        assert_eq!(after["oauthAccount"]["accountUuid"], "uuid-A");
        assert_eq!(after["oauthAccount"]["emailAddress"], "a@x.com");
        // ...but B's projects/mcp/theme are INTACT (the A1 guarantee).
        assert_eq!(after["projects"]["/keep/me"]["trust"], true);
        assert_eq!(after["mcpServers"]["sessionwiki"]["command"], "sessionwiki");
        assert_eq!(after["theme"], "light");
        let creds: Value =
            serde_json::from_slice(&std::fs::read(pb.claude_credentials()).unwrap()).unwrap();
        assert_eq!(creds["claudeAiOauth"]["subscriptionType"], "max");

        let id = Claude.identity(&pb).unwrap().unwrap();
        assert_eq!(id.account_id, "uuid-A");
        assert_eq!(id.email.as_deref(), Some("a@x.com"));
    }
}
