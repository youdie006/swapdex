//! Claude Code 2.1.271–2.1.274 refresh exclusion uses two directory locks.
//! This guard participates in that protocol, including 60-second stale lock
//! recovery after a crashed owner. Like the native protocol, final path check
//! and rmdir are not atomic against an adversarial concurrent replacement.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, FileTimes, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

const HEARTBEAT: Duration = Duration::from_secs(5);
const STALE: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum LockError {
    Busy { path: PathBuf },
    Invalid { path: PathBuf, reason: &'static str },
    Io { path: PathBuf, source: io::Error },
    LostOwnership { path: PathBuf },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { path } => write!(f, "Claude refresh lock is busy: {}", path.display()),
            Self::Invalid { path, reason } => {
                write!(
                    f,
                    "invalid Claude refresh lock at {}: {reason}",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(f, "Claude refresh lock I/O at {}: {source}", path.display())
            }
            Self::LostOwnership { path } => {
                write!(f, "Claude refresh lock ownership lost: {}", path.display())
            }
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn identity(meta: &Metadata) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    Ok(Identity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

#[cfg(windows)]
fn identity(meta: &Metadata) -> io::Result<Identity> {
    use std::os::windows::fs::MetadataExt;
    let device = meta
        .volume_serial_number()
        .ok_or_else(|| io::Error::other("directory volume identity unavailable"))?;
    let inode = meta
        .file_index()
        .ok_or_else(|| io::Error::other("directory file identity unavailable"))?;
    Ok(Identity {
        device: u64::from(device),
        inode,
    })
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StaleSnapshot {
    identity: Identity,
    modified: SystemTime,
}

impl StaleSnapshot {
    fn inspect(path: &Path) -> Result<Option<Self>, LockError> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LockError::Io {
                    path: path.into(),
                    source,
                })
            }
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(LockError::Invalid {
                path: path.into(),
                reason: "lock path is not a plain directory",
            });
        }
        let identity = identity(&meta).map_err(|source| LockError::Io {
            path: path.into(),
            source,
        })?;
        let modified = meta.modified().map_err(|source| LockError::Io {
            path: path.into(),
            source,
        })?;
        Ok(Some(Self { identity, modified }))
    }

    fn is_stale(self, stale: Duration) -> bool {
        SystemTime::now()
            .duration_since(self.modified)
            .is_ok_and(|age| age > stale)
    }

    fn reclaim_if_unchanged(self, path: &Path, stale: Duration) -> Result<bool, LockError> {
        if !self.is_stale(stale)
            || StaleSnapshot::inspect(path)?
                .is_none_or(|current| current != self || !current.is_stale(stale))
        {
            return Ok(false);
        }
        // rmdir only succeeds for an empty directory. We never recurse into
        // content that may belong to a native process or a new successor.
        match fs::remove_dir(path) {
            Ok(()) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                Ok(false)
            }
            Err(source) => Err(LockError::Io {
                path: path.into(),
                source,
            }),
        }
    }
}

#[cfg(windows)]
fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

struct OwnedDirectory {
    path: PathBuf,
    handle: File,
    identity: Identity,
}

impl OwnedDirectory {
    fn create(path: PathBuf, stale: Duration) -> Result<Self, LockError> {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let Some(observed) = StaleSnapshot::inspect(&path)? else {
                    return Err(LockError::Busy { path });
                };
                if !observed.reclaim_if_unchanged(&path, stale)? {
                    return Err(LockError::Busy { path });
                }
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        StaleSnapshot::inspect(&path)?;
                        return Err(LockError::Busy { path });
                    }
                    Err(source) => return Err(LockError::Io { path, source }),
                }
            }
            Err(source) => return Err(LockError::Io { path, source }),
        }

        let result = (|| {
            let handle = open_directory(&path).map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
            let meta = handle.metadata().map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
            let identity = identity(&meta).map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
            let owned = Self {
                path,
                handle,
                identity,
            };
            if !owned.matches_path()? {
                return Err(LockError::LostOwnership {
                    path: owned.path.clone(),
                });
            }
            Ok(owned)
        })();
        // A failed open or identity check cannot prove ownership of the path;
        // leave it for the native client rather than remove a possible successor.
        result
    }

    fn matches_path(&self) -> Result<bool, LockError> {
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => identity(&meta)
                .map(|current| current == self.identity)
                .map_err(|source| LockError::Io {
                    path: self.path.clone(),
                    source,
                }),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(LockError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn is_fresh(&self, stale: Duration) -> Result<bool, LockError> {
        if !self.matches_path()? {
            return Ok(false);
        }
        let modified = self
            .handle
            .metadata()
            .and_then(|meta| meta.modified())
            .map_err(|source| LockError::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok(SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO)
            < stale)
    }

    fn heartbeat(&self) -> Result<(), LockError> {
        self.handle
            .set_times(FileTimes::new().set_modified(SystemTime::now()))
            .map_err(|source| LockError::Io {
                path: self.path.clone(),
                source,
            })
    }

    fn release(&self) {
        // Identity is checked immediately before rmdir. POSIX cannot make a
        // path-based inode comparison and rmdir one atomic operation; an
        // adversarial concurrent replacement remains outside this guarantee.
        if self.matches_path().unwrap_or(false) {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

struct State {
    custom: OwnedDirectory,
    legacy: OwnedDirectory,
    stale: Duration,
    lost: AtomicBool,
}

impl State {
    fn is_owned(&self) -> Result<bool, LockError> {
        if self.lost.load(Ordering::Acquire) {
            return Ok(false);
        }
        Ok(self.custom.is_fresh(self.stale)? && self.legacy.is_fresh(self.stale)?)
    }
}

/// Holds Claude's custom and legacy refresh directory locks through one
/// read/exchange/write operation. Keep it alive until the credential write is
/// complete, and recheck `is_owned` before exchanging or writing.
pub struct NativeRefreshLock {
    state: Arc<State>,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
}

impl NativeRefreshLock {
    /// Acquires both native locks in order, without waiting. An unchanged,
    /// empty lock older than Claude's stale threshold may be reclaimed.
    pub fn try_acquire(storage_dir: &Path) -> Result<Self, LockError> {
        Self::try_acquire_with_timing(storage_dir, HEARTBEAT, STALE)
    }

    fn try_acquire_with_timing(
        storage_dir: &Path,
        heartbeat: Duration,
        stale: Duration,
    ) -> Result<Self, LockError> {
        let storage = fs::canonicalize(storage_dir).map_err(|source| LockError::Io {
            path: storage_dir.to_path_buf(),
            source,
        })?;
        if !storage.is_dir() {
            return Err(LockError::Invalid {
                path: storage,
                reason: "credential storage is not a directory",
            });
        }
        let custom = OwnedDirectory::create(storage.join(".oauth_refresh.lock"), stale)?;
        let mut legacy_path = OsString::from(storage.as_os_str());
        legacy_path.push(".lock");
        let legacy = match OwnedDirectory::create(PathBuf::from(legacy_path), stale) {
            Ok(legacy) => legacy,
            Err(error) => {
                custom.release();
                return Err(error);
            }
        };
        let state = Arc::new(State {
            custom,
            legacy,
            stale,
            lost: AtomicBool::new(false),
        });
        let (stop, rx) = mpsc::channel();
        let thread_state = Arc::clone(&state);
        let heartbeat_thread = thread::Builder::new()
            .name("claude-refresh-lock-heartbeat".into())
            .spawn(move || {
                while rx.recv_timeout(heartbeat) == Err(mpsc::RecvTimeoutError::Timeout) {
                    if !matches!(thread_state.is_owned(), Ok(true))
                        || thread_state.custom.heartbeat().is_err()
                        || thread_state.legacy.heartbeat().is_err()
                        || !matches!(thread_state.is_owned(), Ok(true))
                    {
                        thread_state.lost.store(true, Ordering::Release);
                        break;
                    }
                }
            })
            .map_err(|source| {
                state.custom.release();
                state.legacy.release();
                LockError::Io {
                    path: storage,
                    source,
                }
            })?;
        Ok(Self {
            state,
            stop: Some(stop),
            heartbeat: Some(heartbeat_thread),
        })
    }

    pub fn is_owned(&self) -> Result<bool, LockError> {
        self.state.is_owned()
    }

    pub fn ensure_owned(&self) -> Result<(), LockError> {
        if self.is_owned()? {
            Ok(())
        } else {
            Err(LockError::LostOwnership {
                path: self.state.custom.path.clone(),
            })
        }
    }
}

impl Drop for NativeRefreshLock {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        self.state.legacy.release();
        self.state.custom.release();
    }
}

#[cfg(test)]
mod tests {
    use super::{LockError, NativeRefreshLock, StaleSnapshot};
    use std::ffi::OsString;
    use std::fs::{self, File, FileTimes};
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, SystemTime};

    fn paths(storage: &Path) -> (PathBuf, PathBuf) {
        let storage = storage.canonicalize().unwrap();
        let custom = storage.join(".oauth_refresh.lock");
        let mut legacy = OsString::from(storage.as_os_str());
        legacy.push(".lock");
        (custom, PathBuf::from(legacy))
    }

    #[test]
    fn takes_both_native_locks_and_releases_them() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);

        let guard = NativeRefreshLock::try_acquire(&storage).unwrap();
        assert!(custom.is_dir());
        assert!(legacy.is_dir());
        assert!(guard.is_owned().unwrap());
        guard.ensure_owned().unwrap();
        assert!(matches!(
            NativeRefreshLock::try_acquire(&storage).err().unwrap(),
            LockError::Busy { .. }
        ));

        drop(guard);
        assert!(!custom.exists());
        assert!(!legacy.exists());
        let successor = NativeRefreshLock::try_acquire(&storage).unwrap();
        assert!(successor.is_owned().unwrap());
    }

    #[test]
    fn second_lock_contention_cleans_up_only_first_lock() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        fs::create_dir(&legacy).unwrap();

        assert!(matches!(
            NativeRefreshLock::try_acquire(&storage).err().unwrap(),
            LockError::Busy { .. }
        ));
        assert!(!custom.exists());
        assert!(legacy.is_dir());
    }

    #[test]
    fn heartbeat_keeps_both_directory_mtimes_fresh() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        let guard = NativeRefreshLock::try_acquire_with_timing(
            &storage,
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
        .unwrap();
        let first_custom = fs::metadata(&custom).unwrap().modified().unwrap();
        let first_legacy = fs::metadata(&legacy).unwrap().modified().unwrap();

        thread::sleep(Duration::from_millis(100));
        assert!(fs::metadata(&custom).unwrap().modified().unwrap() > first_custom);
        assert!(fs::metadata(&legacy).unwrap().modified().unwrap() > first_legacy);
        assert!(guard.is_owned().unwrap());
    }

    #[test]
    fn old_guard_never_removes_replacement_directories() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        let guard = NativeRefreshLock::try_acquire_with_timing(
            &storage,
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
        .unwrap();
        fs::remove_dir(&custom).unwrap();
        fs::remove_dir(&legacy).unwrap();
        fs::create_dir(&custom).unwrap();
        fs::create_dir(&legacy).unwrap();

        assert!(!guard.is_owned().unwrap());
        assert!(matches!(
            guard.ensure_owned().err().unwrap(),
            LockError::LostOwnership { .. }
        ));
        drop(guard);
        assert!(custom.is_dir());
        assert!(legacy.is_dir());
    }

    #[test]
    fn stale_custom_and_legacy_locks_are_recovered_after_crash() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        fs::create_dir(&custom).unwrap();
        fs::create_dir(&legacy).unwrap();
        for path in [&custom, &legacy] {
            File::open(path)
                .unwrap()
                .set_times(
                    FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)),
                )
                .unwrap();
        }

        let guard = NativeRefreshLock::try_acquire(&storage).unwrap();
        assert!(guard.is_owned().unwrap());
        assert!(custom.is_dir());
        assert!(legacy.is_dir());
        drop(guard);
        assert!(!custom.exists());
        assert!(!legacy.exists());
    }

    #[test]
    fn fresh_native_heartbeat_stays_busy() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        fs::create_dir(&custom).unwrap();

        assert!(matches!(
            NativeRefreshLock::try_acquire_with_timing(
                &storage,
                Duration::from_millis(20),
                Duration::from_secs(1)
            )
            .err()
            .unwrap(),
            LockError::Busy { .. }
        ));
        assert!(custom.is_dir());
        assert!(!legacy.exists());
    }

    #[test]
    fn future_directory_mtime_is_never_assumed_stale() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        fs::create_dir(&custom).unwrap();
        File::open(&custom)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(120)))
            .unwrap();

        assert!(matches!(
            NativeRefreshLock::try_acquire(&storage).err().unwrap(),
            LockError::Busy { .. }
        ));
        assert!(custom.is_dir());
        assert!(!legacy.exists());
    }

    #[test]
    fn resumed_native_heartbeat_invalidates_a_stale_observation() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, _) = paths(&storage);
        fs::create_dir(&custom).unwrap();
        let handle = File::open(&custom).unwrap();
        handle
            .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
            .unwrap();
        let stale = StaleSnapshot::inspect(&custom).unwrap().unwrap();

        handle
            .set_times(FileTimes::new().set_modified(SystemTime::now()))
            .unwrap();
        assert!(!stale
            .reclaim_if_unchanged(&custom, Duration::from_secs(60))
            .unwrap());
        assert!(custom.is_dir());
    }

    #[test]
    fn replacement_native_directory_invalidates_a_stale_observation() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, _) = paths(&storage);
        fs::create_dir(&custom).unwrap();
        File::open(&custom)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
            .unwrap();
        let stale = StaleSnapshot::inspect(&custom).unwrap().unwrap();

        fs::remove_dir(&custom).unwrap();
        fs::create_dir(&custom).unwrap();
        assert!(!stale
            .reclaim_if_unchanged(&custom, Duration::from_secs(60))
            .unwrap());
        assert!(custom.is_dir());
    }

    #[test]
    fn successor_after_reclaim_wins_the_new_mkdir() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        fs::create_dir(&custom).unwrap();
        File::open(&custom)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
            .unwrap();
        let stale = StaleSnapshot::inspect(&custom).unwrap().unwrap();
        assert!(stale
            .reclaim_if_unchanged(&custom, Duration::from_secs(60))
            .unwrap());

        fs::create_dir(&custom).unwrap();
        assert!(matches!(
            NativeRefreshLock::try_acquire(&storage).err().unwrap(),
            LockError::Busy { .. }
        ));
        assert!(custom.is_dir());
        assert!(!legacy.exists());
    }

    #[test]
    fn nonempty_stale_directory_is_not_removed() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let (custom, legacy) = paths(&storage);
        fs::create_dir(&custom).unwrap();
        fs::write(custom.join("native-content"), b"keep").unwrap();
        File::open(&custom)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
            .unwrap();

        assert!(matches!(
            NativeRefreshLock::try_acquire(&storage).err().unwrap(),
            LockError::Busy { .. }
        ));
        assert_eq!(fs::read(custom.join("native-content")).unwrap(), b"keep");
        assert!(!legacy.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_lock_path_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        let target = root.path().join("target");
        fs::create_dir(&storage).unwrap();
        fs::create_dir(&target).unwrap();
        let (custom, legacy) = paths(&storage);
        symlink(&target, &custom).unwrap();

        assert!(matches!(
            NativeRefreshLock::try_acquire(&storage).err().unwrap(),
            LockError::Invalid { .. }
        ));
        assert!(custom.is_symlink());
        assert!(target.is_dir());
        assert!(!legacy.exists());
    }
}
