//! Durable selection of the one native store that owns a Claude login.

use crate::{live_login::LoginIdentity, paths::Paths};
use anyhow::{bail, Context};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

pub(crate) const RECORD: &str = ".swapdex-claude-authority.json";
const ASSOCIATION_LOCK: &str = ".swapdex-claude-authority.lock";

/// Serializes only the short descriptor association. The live native source's
/// refresh remains independent: source credentials are checked, never written.
struct AssociationLock {
    file: File,
    path: PathBuf,
}

impl AssociationLock {
    fn acquire(slot: &Path) -> anyhow::Result<Self> {
        let path = slot.join(ASSOCIATION_LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| {
                format!("open Claude authority association lock: {}", path.display())
            })?;
        let guard = Self { file, path };
        guard.ensure_owned()?;
        guard
            .file
            .lock_exclusive()
            .context("lock Claude authority association")?;
        guard.ensure_owned()?;
        Ok(guard)
    }

    fn ensure_owned(&self) -> anyhow::Result<()> {
        let opened = self.file.metadata()?;
        let named = std::fs::symlink_metadata(&self.path)?;
        if !opened.is_file()
            || !named.file_type().is_file()
            || opened.uid() != unsafe { libc::geteuid() }
            || opened.mode() & 0o777 != 0o600
            || opened.nlink() != 1
            || (opened.dev(), opened.ino()) != (named.dev(), named.ino())
        {
            bail!("Claude authority association lock is not a private owned regular file");
        }
        Ok(())
    }
}

impl Drop for AssociationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    version: u8,
    storage_dir: PathBuf,
    identity_path: PathBuf,
    securestorage_key: Option<String>,
    account_uuid: String,
    organization_uuid: String,
    // Proof of the generation that was shared at association time. It is not
    // a provider family ID and does not have to match the authority after it
    // rotates. It fences an unexpected new login in the obsolete slot copy.
    linked_refresh_fingerprint: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Authority {
    pub(crate) storage_dir: PathBuf,
    pub(crate) identity_path: PathBuf,
    pub(crate) securestorage_key: Option<String>,
    slot: PathBuf,
    record: Option<Vec<u8>>,
    identity_before: Option<LoginIdentity>,
    slot_identity_before: Option<LoginIdentity>,
}

fn regular(path: &Path) -> anyhow::Result<Vec<u8>> {
    crate::atomic::read_regular(path)
}

fn record(slot: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let path = slot.join(RECORD);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("inspect Claude credential authority"),
        Ok(_) => regular(&path).map(Some),
    }
}

fn identity(bytes: &[u8]) -> Option<LoginIdentity> {
    crate::live_login::identity_from_credential(bytes, "claude-code")
}

fn refresh_fingerprint(bytes: &[u8]) -> Option<String> {
    crate::refresh::refresh_token_identity(bytes, "claude-code")
}

pub(crate) fn resolve(paths: &Paths, slot: &Path) -> anyhow::Result<Authority> {
    resolve_inner(paths, slot, false)
}

fn resolve_inner(paths: &Paths, slot: &Path, authentication: bool) -> anyhow::Result<Authority> {
    if !crate::proc::native_path_allowed(paths, slot) {
        bail!("Claude credential authority is outside the selected home");
    }
    let record = record(slot)?;
    let Some(bytes) = record.as_deref() else {
        return Ok(Authority {
            storage_dir: slot.to_path_buf(),
            identity_path: slot.join(".claude.json"),
            securestorage_key: Some(slot.to_string_lossy().into_owned()),
            slot: slot.to_path_buf(),
            record: None,
            identity_before: regular(&slot.join(".claude.json"))
                .ok()
                .and_then(|bytes| identity(&bytes)),
            slot_identity_before: None,
        });
    };
    let binding: Binding =
        serde_json::from_slice(bytes).context("Claude credential authority record is invalid")?;
    if binding.version != 1
        || binding.account_uuid.is_empty()
        || binding.organization_uuid.is_empty()
        || !crate::proc::native_path_allowed(paths, &binding.storage_dir)
        || !crate::proc::native_path_allowed(paths, &binding.identity_path)
    {
        bail!("Claude credential authority record is not usable");
    }
    let selected_storage = binding
        .securestorage_key
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.home().join(".claude"));
    if selected_storage != binding.storage_dir {
        bail!("Claude credential authority storage and native launch disagree");
    }
    let expected = LoginIdentity::Claude {
        account_uuid: binding.account_uuid,
        organization_uuid: binding.organization_uuid,
    };
    let source_before = match regular(&binding.identity_path) {
        Ok(bytes) => Some(bytes),
        Err(_)
            if authentication
                && std::fs::symlink_metadata(&binding.identity_path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let source_identity = source_before.as_deref().and_then(identity);
    let slot_before = regular(&slot.join(".claude.json"))?;
    // A deliberate logout removes oauthAccount. It must disable inference and
    // renewal, while still allowing native auth login to restore this store.
    // A different known account remains protected from this slot's auth commands.
    if (source_identity.as_ref() != Some(&expected)
        && !(authentication && source_identity.is_none()))
        || identity(&slot_before).as_ref() != Some(&expected)
    {
        bail!("Claude credential authority identity changed; refusing the obsolete slot copy");
    }
    let old_copy = crate::adapters::claude::slot_credential(slot)
        .map_err(|_| anyhow::anyhow!("cannot verify the linked Claude slot"))?;
    if refresh_fingerprint(old_copy.bytes()).as_deref()
        != Some(binding.linked_refresh_fingerprint.as_str())
    {
        bail!("Claude slot was signed in again; credential authority needs reconciliation");
    }
    Ok(Authority {
        storage_dir: binding.storage_dir,
        identity_path: binding.identity_path,
        securestorage_key: binding.securestorage_key,
        slot: slot.to_path_buf(),
        record,
        identity_before: source_identity,
        slot_identity_before: identity(&slot_before),
    })
}

pub(crate) fn resolve_for_authentication(paths: &Paths, slot: &Path) -> anyhow::Result<Authority> {
    resolve_inner(paths, slot, true)
}

impl Authority {
    pub(crate) fn is_current(&self, paths: &Paths) -> bool {
        resolve(paths, &self.slot).is_ok_and(|current| current == *self)
    }

    pub(crate) fn is_bound(&self) -> bool {
        self.record.is_some()
    }

    pub(crate) fn identity(&self) -> Option<LoginIdentity> {
        self.identity_before.clone()
    }

    /// Empty is intentional: native Claude uses the default file store and
    /// bare Keychain service even when CLAUDE_CONFIG_DIR names another slot.
    pub(crate) fn securestorage_override(&self) -> &str {
        self.securestorage_key.as_deref().unwrap_or("")
    }

    pub(crate) fn configure_launch(
        &self,
        paths: &Paths,
        command: &mut std::process::Command,
        authentication: bool,
    ) -> anyhow::Result<()> {
        let current = if authentication {
            resolve_for_authentication(paths, &self.slot).is_ok_and(|current| current == *self)
        } else {
            self.is_current(paths)
        };
        if !current {
            bail!("Claude login changed before launch");
        }
        if !self.is_bound() {
            return Ok(());
        }
        command.env(
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            self.securestorage_override(),
        );
        if authentication {
            // Native login/logout must update the identity beside the same
            // credential authority, rather than only the session-slot label.
            if self.identity_path == paths.home().join(".claude.json") {
                command.env_remove("CLAUDE_CONFIG_DIR");
            } else {
                let config = self
                    .identity_path
                    .parent()
                    .context("Claude identity has no parent")?;
                command.env("CLAUDE_CONFIG_DIR", config);
            }
        }
        Ok(())
    }

    pub(crate) fn read(
        &self,
        paths: &Paths,
    ) -> Result<crate::adapters::claude::SlotCredential, crate::adapters::claude::KeychainReadError>
    {
        let credential = crate::adapters::claude::native_credential_detail(
            paths,
            &self.storage_dir,
            self.securestorage_key.as_deref(),
        )?;
        if !self.is_current(paths) {
            return Err(crate::adapters::claude::KeychainReadError::Missing);
        }
        Ok(credential)
    }

    /// Caller owns the native refresh locks and checks the input generation.
    pub(crate) fn write(
        &self,
        paths: &Paths,
        source: crate::adapters::claude::SlotCredentialSource,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        if !self.is_current(paths) {
            bail!("Claude credential authority changed before persistence");
        }
        match source {
            crate::adapters::claude::SlotCredentialSource::File => {
                crate::atomic::write_secret(&self.storage_dir.join(".credentials.json"), bytes)
            }
            crate::adapters::claude::SlotCredentialSource::Keychain => {
                if paths.sandboxed() {
                    bail!("a rooted Claude authority cannot write the real Keychain");
                }
                crate::adapters::claude::native_keychain_write(
                    self.securestorage_key.as_deref(),
                    bytes,
                )
            }
        }
    }
}

pub(crate) fn credential(
    paths: &Paths,
    slot: &Path,
) -> Result<crate::adapters::claude::SlotCredential, crate::adapters::claude::KeychainReadError> {
    resolve(paths, slot)
        .map_err(|_| crate::adapters::claude::KeychainReadError::Missing)?
        .read(paths)
}

/// Establish a binding only from a running native owner and an exact shared
/// refresh generation. Read-only callers never create authority records.
pub(crate) fn reconcile_live(paths: &Paths, slot: &Path) -> anyhow::Result<bool> {
    if record(slot)?.is_some() {
        resolve(paths, slot)?;
        return Ok(false);
    }
    let mut processes = crate::proc::running_native_login_processes(paths, "claude-code");
    // An existing process in the old slot cannot be redirected by changing a
    // marker: its environment is already fixed. Keep the old ownership guard.
    if processes.iter().any(|p| p.source_dir == slot) {
        return Ok(false);
    }
    processes.sort_by(|a, b| a.source_dir.cmp(&b.source_dir));
    processes.dedup();
    let slot_credential = crate::adapters::claude::slot_credential(slot)
        .map_err(|_| anyhow::anyhow!("cannot read the selected Claude slot"))?;
    let shared = refresh_fingerprint(slot_credential.bytes());
    let selected_identity = regular(&slot.join(".claude.json"))
        .ok()
        .and_then(|bytes| identity(&bytes));
    if shared.is_none() || selected_identity.is_none() {
        return Ok(false);
    }
    let mut candidates = processes.into_iter().filter(|p| {
        regular(&p.identity_path)
            .ok()
            .and_then(|bytes| identity(&bytes))
            == selected_identity
            && crate::adapters::claude::native_credentials(
                paths,
                &p.source_dir,
                p.claude_keychain_key.as_deref(),
            )
            .as_deref()
            .and_then(refresh_fingerprint)
                == shared
    });
    let Some(candidate) = candidates.next() else {
        return Ok(false);
    };
    if candidates.next().is_some() {
        bail!("multiple native stores hold the same Claude refresh generation");
    }
    bind_to_native(paths, slot, &candidate)
}

pub(crate) fn bind_to_native(
    paths: &Paths,
    slot: &Path,
    native: &crate::proc::NativeLoginProcess,
) -> anyhow::Result<bool> {
    if !native.supports_refresh_locks {
        return Ok(false);
    }
    if record(slot)?.is_some() {
        resolve(paths, slot)?;
        return Ok(false);
    }
    if !crate::proc::native_path_allowed(paths, slot)
        || !crate::proc::native_path_allowed(paths, &native.source_dir)
        || !crate::proc::native_path_allowed(paths, &native.identity_path)
    {
        return Ok(false);
    }
    let slot_real = std::fs::canonicalize(slot)?;
    let native_real = std::fs::canonicalize(&native.source_dir)?;
    if slot_real == native_real {
        return Ok(false);
    }
    let association = AssociationLock::acquire(slot)?;
    if record(slot)?.is_some() {
        resolve(paths, slot)?;
        return Ok(false);
    }
    if std::fs::canonicalize(slot)? != slot_real
        || std::fs::canonicalize(&native.source_dir)? != native_real
    {
        bail!("Claude credential authority path changed during association");
    }
    // An unbound Swapdex refresh can still rotate the slot's token. Take its
    // native-compatible pair, while leaving the live SOURCE pair available to
    // Claude's own renewal. The metadata flock serializes bind callers.
    let slot_refresh = crate::claude_refresh_lock::NativeRefreshLock::try_acquire(&slot_real)?;
    let slot_identity = regular(&slot.join(".claude.json"))?;
    let source_identity = regular(&native.identity_path)?;
    let Some(LoginIdentity::Claude {
        account_uuid,
        organization_uuid,
    }) = identity(&slot_identity)
    else {
        return Ok(false);
    };
    if identity(&slot_identity) != identity(&source_identity) {
        return Ok(false);
    }
    let slot_credential = crate::adapters::claude::slot_credential(slot)
        .map_err(|_| anyhow::anyhow!("cannot read the selected Claude slot"))?;
    let Some(native_credential) = crate::adapters::claude::native_credentials(
        paths,
        &native.source_dir,
        native.claude_keychain_key.as_deref(),
    ) else {
        return Ok(false);
    };
    let Some(shared) = refresh_fingerprint(slot_credential.bytes()) else {
        return Ok(false);
    };
    if refresh_fingerprint(&native_credential).as_deref() != Some(shared.as_str()) {
        return Ok(false);
    }
    if regular(&slot.join(".claude.json"))? != slot_identity
        || regular(&native.identity_path)? != source_identity
        || !crate::adapters::claude::slot_credential(slot)
            .is_ok_and(|current| current == slot_credential)
        || crate::adapters::claude::native_credentials(
            paths,
            &native.source_dir,
            native.claude_keychain_key.as_deref(),
        )
        .as_deref()
            != Some(native_credential.as_slice())
        || record(slot)?.is_some()
    {
        bail!("Claude login changed while selecting its credential authority");
    }
    let binding = Binding {
        version: 1,
        storage_dir: native.source_dir.clone(),
        identity_path: native.identity_path.clone(),
        securestorage_key: native.claude_keychain_key.clone(),
        account_uuid,
        organization_uuid,
        linked_refresh_fingerprint: shared,
    };
    slot_refresh.ensure_owned()?;
    association.ensure_owned()?;
    crate::atomic::write_secret(&slot.join(RECORD), &serde_json::to_vec(&binding)?)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(path: &Path, account: &str, org: &str) {
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "oauthAccount": {"accountUuid": account, "organizationUuid": org}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn credential(dir: &Path, refresh: &str) {
        std::fs::write(
            dir.join(".credentials.json"),
            serde_json::to_vec(&serde_json::json!({"claudeAiOauth": {
                "accessToken": "fixture-access", "refreshToken": refresh,
                "expiresAt": 4102444800000_i64
            }}))
            .unwrap(),
        )
        .unwrap();
    }

    struct Fixture {
        root: tempfile::TempDir,
        paths: Paths,
        slot: PathBuf,
        native: crate::proc::NativeLoginProcess,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths::rooted(root.path());
            let slot = paths.store_dir().join("slots/fixture");
            let source_dir = root.path().join(".claude");
            std::fs::create_dir_all(&slot).unwrap();
            std::fs::create_dir_all(&source_dir).unwrap();
            let identity_path = root.path().join(".claude.json");
            identity(&slot.join(".claude.json"), "account-a", "org-a");
            identity(&identity_path, "account-a", "org-a");
            credential(&slot, "shared-refresh");
            credential(&source_dir, "shared-refresh");
            Self {
                root,
                paths,
                slot,
                native: crate::proc::NativeLoginProcess {
                    source_dir,
                    identity_path,
                    claude_keychain_key: None,
                    supports_refresh_locks: true,
                },
            }
        }
    }

    #[test]
    fn a_proven_copy_retains_native_authority_after_process_exit() {
        let f = Fixture::new();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        // Resolution deliberately has no running-process prerequisite.
        let authority = resolve(&f.paths, &f.slot).unwrap();
        assert_eq!(authority.storage_dir, f.native.source_dir);
        assert_eq!(authority.identity_path, f.native.identity_path);
        assert_eq!(authority.securestorage_key, None, "bare Keychain service");
    }

    #[test]
    fn association_does_not_take_the_native_source_refresh_lock() {
        use std::os::unix::fs::MetadataExt;

        let f = Fixture::new();
        let held = crate::claude_refresh_lock::NativeRefreshLock::try_acquire(&f.native.source_dir)
            .unwrap();
        let custom = f.native.source_dir.join(".oauth_refresh.lock");
        let legacy = f.root.path().join(".claude.lock");
        let before = [
            std::fs::metadata(&custom).unwrap(),
            std::fs::metadata(&legacy).unwrap(),
        ];
        let old_slot = std::fs::read(f.slot.join(".credentials.json")).unwrap();
        let old_source = std::fs::read(f.native.source_dir.join(".credentials.json")).unwrap();

        assert!(bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        held.ensure_owned().unwrap();
        for (path, prior) in [custom, legacy].into_iter().zip(before) {
            let current = std::fs::metadata(path).unwrap();
            assert_eq!((current.dev(), current.ino()), (prior.dev(), prior.ino()));
        }
        assert_eq!(
            std::fs::read(f.slot.join(".credentials.json")).unwrap(),
            old_slot
        );
        assert_eq!(
            std::fs::read(f.native.source_dir.join(".credentials.json")).unwrap(),
            old_source
        );
        credential(&f.native.source_dir, "rotated-while-native-is-open");
        assert_eq!(
            resolve(&f.paths, &f.slot).unwrap().storage_dir,
            f.native.source_dir
        );
    }

    #[test]
    fn association_refuses_a_slot_under_refresh() {
        let f = Fixture::new();
        let held = crate::claude_refresh_lock::NativeRefreshLock::try_acquire(&f.slot).unwrap();
        let old_slot = std::fs::read(f.slot.join(".credentials.json")).unwrap();
        let old_source = std::fs::read(f.native.source_dir.join(".credentials.json")).unwrap();

        assert!(bind_to_native(&f.paths, &f.slot, &f.native).is_err());
        held.ensure_owned().unwrap();
        assert!(!f.slot.join(RECORD).exists());
        assert_eq!(
            std::fs::read(f.slot.join(".credentials.json")).unwrap(),
            old_slot
        );
        assert_eq!(
            std::fs::read(f.native.source_dir.join(".credentials.json")).unwrap(),
            old_source
        );
    }

    #[test]
    fn concurrent_associations_create_one_authority_record() {
        let f = Fixture::new();
        let callers = 8;
        let barrier = std::sync::Barrier::new(callers);
        let wins = std::thread::scope(|scope| {
            let attempts: Vec<_> = (0..callers)
                .map(|_| {
                    let barrier = &barrier;
                    let paths = &f.paths;
                    let slot = &f.slot;
                    let native = &f.native;
                    scope.spawn(move || {
                        barrier.wait();
                        bind_to_native(paths, slot, native).unwrap()
                    })
                })
                .collect();
            attempts
                .into_iter()
                .map(|attempt| usize::from(attempt.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(wins, 1);
        assert_eq!(
            resolve(&f.paths, &f.slot).unwrap().storage_dir,
            f.native.source_dir
        );
    }

    #[test]
    fn a_symlinked_association_lock_is_refused() {
        let f = Fixture::new();
        let target = f.root.path().join("unrelated-file");
        std::fs::write(&target, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&target, f.slot.join(".swapdex-claude-authority.lock")).unwrap();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).is_err());
        assert!(!f.slot.join(RECORD).exists());
        assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn an_overly_permissive_association_lock_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let f = Fixture::new();
        let lock = f.slot.join(".swapdex-claude-authority.lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).is_err());
        assert!(!f.slot.join(RECORD).exists());
    }

    #[test]
    fn an_unverified_native_version_cannot_establish_authority() {
        let mut f = Fixture::new();
        f.native.supports_refresh_locks = false;
        assert!(!bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        assert!(!f.slot.join(RECORD).exists());
    }

    #[test]
    fn independent_refresh_generations_are_not_joined_by_account_name() {
        let f = Fixture::new();
        credential(&f.native.source_dir, "independently-issued-refresh");
        assert!(!bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        assert_eq!(resolve(&f.paths, &f.slot).unwrap().storage_dir, f.slot);
    }

    #[test]
    fn organization_mismatch_cannot_bind_even_with_a_copied_token() {
        let f = Fixture::new();
        identity(&f.native.identity_path, "account-a", "another-org");
        assert!(!bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
    }

    #[test]
    fn changed_identity_fails_closed_instead_of_reactivating_the_slot_copy() {
        let f = Fixture::new();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        identity(&f.native.identity_path, "another-account", "org-a");
        assert!(resolve(&f.paths, &f.slot).is_err());
    }

    #[test]
    fn explicit_login_can_restore_a_logged_out_authority_without_using_the_old_copy() {
        let f = Fixture::new();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        std::fs::write(&f.native.identity_path, b"{\"mcpServers\":{}}").unwrap();
        std::fs::write(f.native.source_dir.join(".credentials.json"), b"{}").unwrap();
        assert!(resolve(&f.paths, &f.slot).is_err());
        let authority = resolve_for_authentication(&f.paths, &f.slot).unwrap();
        let mut command = std::process::Command::new("fixture");
        command.env("CLAUDE_CONFIG_DIR", &f.slot);
        authority
            .configure_launch(&f.paths, &mut command, true)
            .unwrap();
        let values: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(values[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")], None);
        assert_eq!(
            values[std::ffi::OsStr::new("CLAUDE_SECURESTORAGE_CONFIG_DIR")],
            Some(std::ffi::OsStr::new(""))
        );
        identity(&f.native.identity_path, "account-a", "org-a");
        credential(&f.native.source_dir, "new-browser-login");
        assert!(resolve(&f.paths, &f.slot).is_ok());
        identity(&f.native.identity_path, "another-account", "org-a");
        assert!(resolve_for_authentication(&f.paths, &f.slot).is_err());
        std::fs::remove_file(&f.native.identity_path).unwrap();
        assert!(resolve_for_authentication(&f.paths, &f.slot).is_ok());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("missing-identity", &f.native.identity_path).unwrap();
            assert!(resolve_for_authentication(&f.paths, &f.slot).is_err());
        }
    }

    #[test]
    fn a_native_rotation_does_not_reactivate_an_obsolete_slot_refresh() {
        let f = Fixture::new();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        credential(&f.native.source_dir, "rotated-by-authority");
        assert_eq!(
            resolve(&f.paths, &f.slot).unwrap().storage_dir,
            f.native.source_dir
        );
        assert!(f.root.path().exists());
    }

    #[test]
    fn renewal_reads_and_writes_the_authority_and_preserves_the_old_copy() {
        let f = Fixture::new();
        let old_copy = std::fs::read(f.slot.join(".credentials.json")).unwrap();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        let authority = resolve(&f.paths, &f.slot).unwrap();
        assert!(authority.is_bound());
        assert_eq!(authority.securestorage_override(), "");
        let before = authority.read(&f.paths).unwrap();
        let renewed = crate::refresh::merge_response(
            before.bytes(),
            r#"{"access_token":"renewed-access","refresh_token":"renewed-refresh","expires_in":3600}"#,
            1_800_000_000_000,
        )
        .unwrap();
        let lock =
            crate::claude_refresh_lock::NativeRefreshLock::try_acquire(&authority.storage_dir)
                .unwrap();
        lock.ensure_owned().unwrap();
        authority
            .write(&f.paths, before.source(), &renewed)
            .unwrap();
        assert_eq!(
            super::credential(&f.paths, &f.slot).unwrap().bytes(),
            renewed
        );
        assert_eq!(
            std::fs::read(f.slot.join(".credentials.json")).unwrap(),
            old_copy
        );
    }

    #[test]
    fn launching_a_linked_slot_preserves_session_home_and_uses_native_auth() {
        let f = Fixture::new();
        assert!(bind_to_native(&f.paths, &f.slot, &f.native).unwrap());
        let authority = resolve(&f.paths, &f.slot).unwrap();
        let mut command = std::process::Command::new("fixture");
        command.env("CLAUDE_CONFIG_DIR", &f.slot);
        authority
            .configure_launch(&f.paths, &mut command, false)
            .unwrap();
        let values: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            values[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")],
            Some(f.slot.as_os_str())
        );
        assert_eq!(
            values[std::ffi::OsStr::new("CLAUDE_SECURESTORAGE_CONFIG_DIR")],
            Some(std::ffi::OsStr::new(""))
        );
        authority
            .configure_launch(&f.paths, &mut command, true)
            .unwrap();
        let values: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(values[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")], None);
    }
}
