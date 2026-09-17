//! Coordinate Claude usage lookups across quota commands and proxy processes.
//!
//! A 429 is a refusal from the usage endpoint, not evidence that an account is
//! spent. Keep one private deadline per access credential so repeated reads do
//! not turn that refusal into a new burst after every process restart.

use crate::paths::Paths;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOCK_WAIT: Duration = Duration::from_secs(20);
const MAX_DELAY: u64 = 24 * 60 * 60;
const FALLBACK: [u64; 5] = [60, 120, 240, 480, 900];

/// The transport result of one granted lookup. Only an HTTP response changes
/// throttle history; a transport failure keeps that history for the next try.
pub(crate) enum Response<T> {
    Http {
        value: T,
        status: u32,
        retry_after: Option<u64>,
    },
    Transport(T),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    failures: u8,
    throttled_at: i64,
    retry_at: i64,
}

impl Record {
    fn validate(self, now: i64) -> Result<Self, String> {
        if self.version != 1 || !(1..=5).contains(&self.failures) || self.throttled_at < 0 {
            return Err("invalid usage lookup backoff state".into());
        }
        let delay = self
            .retry_at
            .checked_sub(self.throttled_at)
            .ok_or("invalid usage lookup backoff state")?;
        if delay < fallback(self.failures) as i64
            || delay > MAX_DELAY as i64
            || self.throttled_at > now.saturating_add(60)
        {
            return Err("invalid usage lookup backoff state".into());
        }
        Ok(self)
    }
}

fn fallback(failures: u8) -> u64 {
    FALLBACK[(failures - 1) as usize]
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn paths_for(paths: &Paths, token: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = paths.store_dir().join("usage-backoff");
    let digest: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let state = dir.join(format!("{digest}.json"));
    let lock = dir.join(format!("{digest}.lock"));
    (dir, state, lock)
}

fn prepare_dir(paths: &Paths, dir: &Path) -> Result<(), String> {
    let store = paths.store_dir();
    fs::create_dir_all(&store).map_err(|error| format!("create usage store: {error}"))?;
    crate::atomic::refuse_symlink_below(&store, dir)
        .map_err(|error| format!("check usage store: {error}"))?;
    fs::set_permissions(&store, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("protect usage store: {error}"))?;
    fs::create_dir_all(dir).map_err(|error| format!("create usage backoff directory: {error}"))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("protect usage backoff directory: {error}"))?;
    Ok(())
}

fn lock_for(paths: &Paths, token: &str, timeout: Duration) -> Result<(File, PathBuf), String> {
    let (dir, state, lock) = paths_for(paths, token);
    prepare_dir(paths, &dir)?;
    crate::atomic::refuse_symlink_below(&paths.store_dir(), &lock)
        .map_err(|error| format!("check usage lookup lock: {error}"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock)
        .map_err(|error| format!("open usage lookup lock: {error}"))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("protect usage lookup lock: {error}"))?;

    let started = Instant::now();
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok((file, state)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    return Err("usage lookup coordination lock timed out".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("lock usage lookup: {error}")),
        }
    }
}

fn read_state(path: &Path, now: i64) -> Result<Option<Record>, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect usage lookup state: {error}")),
        Ok(_) => {}
    }
    let bytes = crate::atomic::read_regular(path)
        .map_err(|error| format!("read usage lookup state: {error}"))?;
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|_| "invalid usage lookup backoff state".to_string())?;
    record.validate(now).map(Some)
}

fn write_state(path: &Path, record: Record) -> Result<(), String> {
    let bytes = serde_json::to_vec(&record)
        .map_err(|error| format!("encode usage lookup state: {error}"))?;
    crate::atomic::write_secret(path, &bytes)
        .map_err(|error| format!("write usage lookup state: {error}"))
}

fn clear_state(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("clear usage lookup state: {error}")),
    }
}

/// Returns `None` while the deadline is active. The caller's closure runs at
/// most once, while this credential's stable lock is held; another token has a
/// different lock and can make progress independently.
pub(crate) fn coordinate<T>(
    paths: &Paths,
    token: &str,
    attempt: impl FnOnce() -> Response<T>,
) -> Result<Option<T>, String> {
    coordinate_with_clock(paths, token, now_secs, LOCK_WAIT, attempt)
}

fn coordinate_with_clock<T>(
    paths: &Paths,
    token: &str,
    clock: impl Fn() -> i64,
    timeout: Duration,
    attempt: impl FnOnce() -> Response<T>,
) -> Result<Option<T>, String> {
    let (_lock, state_path) = lock_for(paths, token, timeout)?;
    let previous = read_state(&state_path, clock())?;
    if previous.is_some_and(|state| clock() < state.retry_at) {
        return Ok(None);
    }

    match attempt() {
        Response::Transport(value) => Ok(Some(value)),
        Response::Http {
            value,
            status: 429,
            retry_after,
        } => {
            // This time is taken after the HTTP response, so a slow request
            // never spends part of its cooldown while it is still in flight.
            let at = clock();
            let failures = previous
                .map(|record| record.failures.saturating_add(1).min(5))
                .unwrap_or(1);
            let delay = fallback(failures).max(retry_after.unwrap_or(0).min(MAX_DELAY));
            let retry_at = at
                .checked_add(delay as i64)
                .ok_or("usage lookup deadline overflow")?;
            write_state(
                &state_path,
                Record {
                    version: 1,
                    failures,
                    throttled_at: at,
                    retry_at,
                },
            )?;
            Ok(Some(value))
        }
        Response::Http { value, .. } => {
            clear_state(&state_path)?;
            Ok(Some(value))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u32) -> Response<u32> {
        Response::Http {
            value: status,
            status,
            retry_after: None,
        }
    }

    fn state(paths: &Paths, token: &str) -> Record {
        let (_, path, _) = paths_for(paths, token);
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn cooldown_survives_calls_and_expiry_escalates_only_on_another_429() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        for (at, expected) in [
            (1000, 60),
            (1060, 120),
            (1180, 240),
            (1420, 480),
            (1900, 900),
            (2800, 900),
        ] {
            assert_eq!(
                coordinate_with_clock(&paths, "secret-a", || at, LOCK_WAIT, || http(429)).unwrap(),
                Some(429)
            );
            let record = state(&paths, "secret-a");
            assert_eq!(record.retry_at - record.throttled_at, expected);
            let before = fs::read(paths_for(&paths, "secret-a").1).unwrap();
            assert_eq!(
                coordinate_with_clock(
                    &paths,
                    "secret-a",
                    || at + 1,
                    LOCK_WAIT,
                    || -> Response<u32> { panic!("cooldown issued HTTP") }
                )
                .unwrap(),
                None
            );
            assert_eq!(fs::read(paths_for(&paths, "secret-a").1).unwrap(), before);
        }
    }

    #[test]
    fn non_429_responses_clear_history_but_transport_failures_keep_it() {
        for status in [200, 201, 401, 403, 500] {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths::rooted(root.path());
            coordinate_with_clock(&paths, "secret-a", || 1000, LOCK_WAIT, || http(429)).unwrap();
            coordinate_with_clock(
                &paths,
                "secret-a",
                || 1060,
                LOCK_WAIT,
                || Response::Transport(0),
            )
            .unwrap();
            assert_eq!(state(&paths, "secret-a").failures, 1);
            assert_eq!(
                coordinate_with_clock(&paths, "secret-a", || 1060, LOCK_WAIT, || http(status))
                    .unwrap(),
                Some(status)
            );
            assert!(!paths_for(&paths, "secret-a").1.exists());
            coordinate_with_clock(&paths, "secret-a", || 1060, LOCK_WAIT, || http(429)).unwrap();
            assert_eq!(state(&paths, "secret-a").failures, 1);
        }
    }

    #[test]
    fn transport_failure_then_429_retains_failure_count_and_tokens_are_independent() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        coordinate_with_clock(&paths, "secret-a", || 1000, LOCK_WAIT, || http(429)).unwrap();
        coordinate_with_clock(
            &paths,
            "secret-a",
            || 1060,
            LOCK_WAIT,
            || Response::Transport(0),
        )
        .unwrap();
        coordinate_with_clock(&paths, "secret-b", || 1001, LOCK_WAIT, || http(429)).unwrap();
        coordinate_with_clock(&paths, "secret-a", || 1060, LOCK_WAIT, || http(429)).unwrap();
        assert_eq!(state(&paths, "secret-a").failures, 2);
        assert_eq!(state(&paths, "secret-b").failures, 1);
        let (dir, state_path, lock_path) = paths_for(&paths, "secret-a");
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!state_path.to_string_lossy().contains("secret-a"));
        assert!(!String::from_utf8_lossy(&fs::read(state_path).unwrap()).contains("secret-a"));
    }

    #[test]
    fn corrupt_or_future_state_fails_closed_without_attempt() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let (dir, state_path, _) = paths_for(&paths, "secret-a");
        fs::create_dir_all(dir).unwrap();
        for bytes in [
            b"not JSON".as_slice(),
            br#"{"version":2,"failures":1,"throttled_at":1000,"retry_at":1060}"#,
            br#"{"version":1,"failures":0,"throttled_at":1000,"retry_at":1060}"#,
            br#"{"version":1,"failures":1,"throttled_at":1061,"retry_at":1121}"#,
            br#"{"version":1,"failures":1,"throttled_at":1000,"retry_at":1001}"#,
            br#"{"version":1,"failures":1,"throttled_at":1000,"retry_at":90000}"#,
        ] {
            fs::write(&state_path, bytes).unwrap();
            assert!(coordinate_with_clock(
                &paths,
                "secret-a",
                || 1000,
                LOCK_WAIT,
                || -> Response<u32> { panic!("invalid state issued HTTP") }
            )
            .is_err());
        }
    }

    #[test]
    fn a_contended_lock_fails_within_the_supplied_bound() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let (lock, _) = lock_for(&paths, "secret-a", LOCK_WAIT).unwrap();
        let started = Instant::now();
        let result: Result<Option<u32>, String> = coordinate_with_clock(
            &paths,
            "secret-a",
            || 1000,
            Duration::from_millis(50),
            || -> Response<u32> { panic!("lock wait issued HTTP") },
        );
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(lock);
    }
}
