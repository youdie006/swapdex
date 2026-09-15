//! Repair Codex rollout metadata written by older Swapdex shims.
//!
//! A rollout can still have an append handle open, so the provider token is
//! replaced at its existing byte offset with an equal-width JSON value. A
//! private, fsynced journal makes an interrupted positional write recoverable.

use crate::paths::Paths;
use crate::slots::Slots;
use fs2::FileExt as _;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{FileExt as _, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_HEADER_BYTES: u64 = 1024 * 1024;
const JOURNAL_VERSION: u32 = 1;
const JOURNAL_DIR: &str = "codex-session-repair";
const REPAIR_LOCK: &str = ".codex-session-repair.lock";
const WRITER_LOCK_DIR: &str = "thread-writer-locks";
const COORDINATION_LOCK: &str = ".coordination.lock";

#[derive(Clone, Copy, Debug, Default)]
pub struct RepairOptions {
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairIssueLevel {
    Deferred,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct RepairIssue {
    pub level: RepairIssueLevel,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl RepairIssue {
    pub fn is_error(&self) -> bool {
        self.level == RepairIssueLevel::Error
    }

    pub fn is_warning(&self) -> bool {
        self.level == RepairIssueLevel::Warning
    }
}

impl fmt::Display for RepairIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.level {
            RepairIssueLevel::Deferred => "deferred",
            RepairIssueLevel::Warning => "warning",
            RepairIssueLevel::Error => "error",
        };
        if let Some(path) = &self.path {
            write!(formatter, "{label}: {}: {}", path.display(), self.message)
        } else {
            write!(formatter, "{label}: {}", self.message)
        }
    }
}

#[derive(Debug, Default)]
pub struct RepairSummary {
    pub scanned: usize,
    pub eligible: usize,
    pub repaired: usize,
    pub skipped: usize,
    pub deferred: usize,
    pub warnings: usize,
    pub errors: usize,
    pub index_rows_repaired: usize,
    pub issues: Vec<RepairIssue>,
}

impl RepairSummary {
    fn issue(&mut self, level: RepairIssueLevel, path: Option<&Path>, message: impl Into<String>) {
        match level {
            RepairIssueLevel::Deferred => self.deferred += 1,
            RepairIssueLevel::Warning => self.warnings += 1,
            RepairIssueLevel::Error => self.errors += 1,
        }
        self.issues.push(RepairIssue {
            level,
            path: path.map(Path::to_path_buf),
            message: message.into(),
        });
    }

    fn error(&mut self, path: Option<&Path>, message: impl Into<String>) {
        self.issue(RepairIssueLevel::Error, path, message);
    }

    fn warning(&mut self, path: Option<&Path>, message: impl Into<String>) {
        self.issue(RepairIssueLevel::Warning, path, message);
    }

    fn defer(&mut self, path: Option<&Path>, message: impl Into<String>) {
        self.issue(RepairIssueLevel::Deferred, path, message);
    }
}

#[derive(Clone, Debug)]
struct CodexHome {
    canonical: PathBuf,
    aliases: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
struct RootAlias {
    home: PathBuf,
    root: PathBuf,
}

#[derive(Clone, Debug)]
struct RolloutFile {
    canonical: PathBuf,
    aliases: Vec<PathBuf>,
    homes: Vec<PathBuf>,
    metadata: fs::Metadata,
}

#[derive(Debug, Deserialize)]
struct HeaderEnvelope<'a> {
    #[serde(default, rename = "type")]
    kind: Option<&'a str>,
    #[serde(default, borrow)]
    payload: Option<&'a RawValue>,
    #[serde(default)]
    id: Option<&'a str>,
    #[serde(default, borrow)]
    timestamp: Option<&'a RawValue>,
    #[serde(default, borrow)]
    instructions: Option<&'a RawValue>,
}

#[derive(Debug, Deserialize)]
struct SessionPayload<'a> {
    #[serde(default)]
    id: Option<&'a str>,
    #[serde(default)]
    session_id: Option<&'a str>,
    #[serde(default, borrow)]
    model_provider: Option<&'a RawValue>,
}

#[derive(Clone, Debug)]
struct ParsedHeader {
    thread_id: String,
    provider: Option<String>,
    token_offset: Option<usize>,
    token: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Journal {
    version: u32,
    rollout_path: PathBuf,
    thread_id: String,
    old_provider: String,
    original_token: String,
    offset: usize,
    header_len: usize,
    original_len: u64,
    prefix_sha256: String,
    suffix_sha256: String,
    #[serde(default)]
    device: u64,
    #[serde(default)]
    inode: u64,
    #[serde(default)]
    modified_secs: Option<u64>,
    #[serde(default)]
    modified_nanos: Option<u32>,
    #[serde(default)]
    accessed_secs: Option<u64>,
    #[serde(default)]
    accessed_nanos: Option<u32>,
    #[serde(default)]
    aliases: Vec<PathBuf>,
}

struct StateDatabases {
    compatible: Vec<StateDatabase>,
}

struct StateDatabase {
    path: PathBuf,
    connection: Connection,
}

struct RepairOutcome {
    header_changed: bool,
    index_rows: usize,
    post_repair_error: Option<String>,
}

/// Repair every eligible rollout reachable from the active, bare, and
/// registered Codex homes. All failures are accumulated so callers can report
/// a truthful nonzero result while leaving journals available for retry.
pub fn repair_codex_sessions(paths: &Paths, options: RepairOptions) -> RepairSummary {
    let mut summary = RepairSummary::default();
    let _repair_lock = if options.dry_run {
        None
    } else {
        match acquire_repair_lock(paths) {
            Ok(lock) => Some(lock),
            Err(message) => {
                summary.error(Some(&paths.store_dir()), message);
                return summary;
            }
        }
    };

    let homes = discover_homes(paths, &mut summary);
    let rollouts = discover_rollouts(&homes, &mut summary);

    if options.dry_run {
        for rollout in rollouts.values() {
            summary.scanned += 1;
            match read_and_parse_rollout(rollout) {
                Ok(parsed) if parsed.provider.as_deref().is_some_and(is_legacy_provider) => {
                    summary.eligible += 1;
                }
                Ok(_) => summary.skipped += 1,
                Err(message) => summary.error(Some(&rollout.canonical), message),
            }
        }
        return summary;
    }

    let mut state = discover_state_databases(&homes, &mut summary);
    let journal_paths = discover_journals(paths, &mut summary);

    let mut completed = BTreeSet::new();
    let mut openai_rollouts = Vec::new();
    for journal_path in journal_paths {
        match read_journal(&journal_path) {
            Ok(journal) => {
                let Some(rollout) = rollouts.get(&journal.rollout_path) else {
                    summary.error(
                        Some(&journal_path),
                        "journal rollout is no longer in a discovered Codex session root",
                    );
                    continue;
                };
                summary.eligible += 1;
                match finish_journaled_repair(&journal_path, &journal, rollout, &mut state) {
                    Ok(outcome) => {
                        summary.repaired += usize::from(outcome.header_changed);
                        summary.index_rows_repaired += outcome.index_rows;
                        if let Some(message) = outcome.post_repair_error {
                            summary.error(Some(&rollout.canonical), message);
                        }
                        completed.insert(rollout.canonical.clone());
                    }
                    Err(RepairFailure::Deferred(message)) => {
                        summary.defer(Some(&rollout.canonical), message);
                    }
                    Err(RepairFailure::Error(message)) => {
                        summary.error(Some(&rollout.canonical), message);
                    }
                }
            }
            Err(message) => summary.error(Some(&journal_path), message),
        }
    }

    for rollout in rollouts.values() {
        summary.scanned += 1;
        if completed.contains(&rollout.canonical) {
            continue;
        }
        let parsed = match read_and_parse_rollout(rollout) {
            Ok(parsed) => parsed,
            Err(message) => {
                summary.error(Some(&rollout.canonical), message);
                continue;
            }
        };
        let Some(old_provider) = parsed.provider.as_deref() else {
            summary.skipped += 1;
            continue;
        };
        if !is_legacy_provider(old_provider) {
            if old_provider == "openai" {
                openai_rollouts.push((rollout.clone(), parsed));
            }
            summary.skipped += 1;
            continue;
        }
        summary.eligible += 1;
        match start_repair(paths, rollout, &parsed, &mut state) {
            Ok(outcome) => {
                summary.repaired += usize::from(outcome.header_changed);
                summary.index_rows_repaired += outcome.index_rows;
                if let Some(message) = outcome.post_repair_error {
                    summary.error(Some(&rollout.canonical), message);
                }
            }
            Err(RepairFailure::Deferred(message)) => {
                summary.defer(Some(&rollout.canonical), message);
            }
            Err(RepairFailure::Error(message)) => {
                summary.error(Some(&rollout.canonical), message);
            }
        }
    }
    reconcile_stale_indexes(paths, &openai_rollouts, &mut state, &mut summary);
    summary
}

fn acquire_repair_lock(paths: &Paths) -> Result<File, String> {
    let store = paths.store_dir();
    fs::create_dir_all(&store).map_err(|error| format!("create repair store: {error}"))?;
    #[cfg(unix)]
    fs::set_permissions(&store, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure repair store: {error}"))?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(store.join(REPAIR_LOCK))
        .map_err(|error| format!("open repair lock: {error}"))?;
    file.try_lock_exclusive()
        .map_err(|error| format!("another Codex session repair is running: {error}"))?;
    Ok(file)
}

fn discover_homes(paths: &Paths, summary: &mut RepairSummary) -> Vec<CodexHome> {
    let mut requested = vec![paths.codex_dir().to_path_buf(), paths.home().join(".codex")];
    match Slots::open_for(paths, "codex") {
        Ok(slots) => requested.extend(slots.list().into_iter().map(|slot| slot.config_dir)),
        Err(error) => summary.error(
            Some(&paths.store_dir().join("slots.json")),
            format!("read Codex slot registry: {error:#}"),
        ),
    }

    let mut homes: BTreeMap<PathBuf, CodexHome> = BTreeMap::new();
    for alias in requested {
        let metadata = match fs::metadata(&alias) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                summary.error(Some(&alias), format!("inspect Codex home: {error}"));
                continue;
            }
        };
        if !metadata.is_dir() {
            summary.error(Some(&alias), "Codex home is not a directory");
            continue;
        }
        let canonical = match fs::canonicalize(&alias) {
            Ok(canonical) => canonical,
            Err(error) => {
                summary.error(Some(&alias), format!("resolve Codex home: {error}"));
                continue;
            }
        };
        let home = homes.entry(canonical.clone()).or_insert_with(|| CodexHome {
            canonical,
            aliases: Vec::new(),
        });
        if !home.aliases.contains(&alias) {
            home.aliases.push(alias);
        }
    }
    homes.into_values().collect()
}

fn discover_rollouts(
    homes: &[CodexHome],
    summary: &mut RepairSummary,
) -> BTreeMap<PathBuf, RolloutFile> {
    let mut roots: BTreeMap<PathBuf, Vec<RootAlias>> = BTreeMap::new();
    for home in homes {
        for alias in &home.aliases {
            for name in ["sessions", "archived_sessions"] {
                let root_alias = alias.join(name);
                match fs::metadata(&root_alias) {
                    Ok(metadata) if metadata.is_dir() => match fs::canonicalize(&root_alias) {
                        Ok(canonical) => roots.entry(canonical).or_default().push(RootAlias {
                            home: home.canonical.clone(),
                            root: root_alias,
                        }),
                        Err(error) => summary
                            .error(Some(&root_alias), format!("resolve session root: {error}")),
                    },
                    Ok(_) => summary.error(Some(&root_alias), "session root is not a directory"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        summary.error(Some(&root_alias), format!("inspect session root: {error}"))
                    }
                }
            }
        }
    }

    let mut rollouts = BTreeMap::new();
    for (root, aliases) in roots {
        walk_rollout_root(&root, &root, &aliases, &mut rollouts, summary);
    }
    rollouts
}

fn walk_rollout_root(
    root: &Path,
    directory: &Path,
    aliases: &[RootAlias],
    rollouts: &mut BTreeMap<PathBuf, RolloutFile>,
    summary: &mut RepairSummary,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            summary.error(Some(directory), format!("read session directory: {error}"));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                summary.error(Some(directory), format!("read session entry: {error}"));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                summary.error(Some(&path), format!("inspect session entry: {error}"));
                continue;
            }
        };
        if file_type.is_dir() {
            walk_rollout_root(root, &path, aliases, rollouts, summary);
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with("rollout-") {
            continue;
        }
        if name.ends_with(".jsonl.zst") {
            summary.skipped += 1;
            summary.warning(
                Some(&path),
                "compressed rollout is outside the safe in-place repair scope",
            );
            continue;
        }
        if !name.ends_with(".jsonl") {
            continue;
        }
        if file_type.is_symlink() || !file_type.is_file() {
            summary.error(Some(&path), "rollout leaf is not a regular file");
            continue;
        }
        let canonical = match fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            Err(error) => {
                summary.error(Some(&path), format!("resolve rollout: {error}"));
                continue;
            }
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                summary.error(Some(&path), "rollout leaf is not a regular file");
                continue;
            }
            Err(error) => {
                summary.error(Some(&path), format!("inspect rollout: {error}"));
                continue;
            }
        };
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => {
                summary.error(Some(&path), "rollout escaped its session root");
                continue;
            }
        };
        let rollout = rollouts
            .entry(canonical.clone())
            .or_insert_with(|| RolloutFile {
                canonical: canonical.clone(),
                aliases: vec![canonical],
                homes: Vec::new(),
                metadata,
            });
        for alias in aliases {
            let rollout_alias = alias.root.join(relative);
            if !rollout.aliases.contains(&rollout_alias) {
                rollout.aliases.push(rollout_alias);
            }
            if !rollout.homes.contains(&alias.home) {
                rollout.homes.push(alias.home.clone());
            }
        }
    }
}

fn discover_state_databases(homes: &[CodexHome], summary: &mut RepairSummary) -> StateDatabases {
    let mut candidates = BTreeSet::new();
    for home in homes {
        let entries = match fs::read_dir(&home.canonical) {
            Ok(entries) => entries,
            Err(error) => {
                summary.error(
                    Some(&home.canonical),
                    format!("read Codex home for state indexes: {error}"),
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.starts_with("state_") || !name.ends_with(".sqlite") {
                continue;
            }
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    summary.error(Some(&path), format!("inspect state index: {error}"));
                    continue;
                }
            };
            if file_type.is_symlink() || !file_type.is_file() {
                summary.error(Some(&path), "state index is not a regular file");
                continue;
            }
            candidates.insert(path);
        }
    }

    let mut compatible = Vec::new();
    for path in candidates {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = match Connection::open_with_flags(&path, flags) {
            Ok(connection) => connection,
            Err(error) => {
                summary.error(Some(&path), format!("read state index: {error}"));
                continue;
            }
        };
        match state_schema_is_compatible(&connection) {
            Ok(true) => compatible.push(StateDatabase { path, connection }),
            Ok(false) => {
                summary.error(Some(&path), "state index has an unsupported threads schema");
            }
            Err(error) => {
                summary.error(Some(&path), format!("read state index: {error}"));
            }
        }
    }
    StateDatabases { compatible }
}

fn state_schema_is_compatible(connection: &Connection) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA table_info(threads)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(["id", "rollout_path", "model_provider"]
        .iter()
        .all(|column| columns.contains(*column)))
}

fn discover_journals(paths: &Paths, summary: &mut RepairSummary) -> Vec<PathBuf> {
    let directory = paths.store_dir().join(JOURNAL_DIR);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            summary.error(Some(&directory), format!("read repair journal: {error}"));
            return Vec::new();
        }
    };
    let mut journals = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        match entry.file_type() {
            Ok(file_type) if file_type.is_file() && !file_type.is_symlink() => journals.push(path),
            Ok(_) => summary.error(Some(&path), "repair journal is not a regular file"),
            Err(error) => summary.error(Some(&path), format!("inspect repair journal: {error}")),
        }
    }
    journals.sort();
    journals
}

fn read_journal(path: &Path) -> Result<Journal, String> {
    read_journal_contents(path, true)
}

fn read_journal_contents(path: &Path, require_pending_name: bool) -> Result<Journal, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("inspect journal: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("journal is not a regular file".to_string());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| format!("open journal: {error}"))?;
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read journal: {error}"))?;
    let journal: Journal =
        serde_json::from_slice(&bytes).map_err(|error| format!("parse journal: {error}"))?;
    if journal.version != JOURNAL_VERSION {
        return Err(format!("unsupported journal version {}", journal.version));
    }
    if !valid_thread_id(&journal.thread_id) || !is_legacy_provider(&journal.old_provider) {
        return Err("journal identity is invalid".to_string());
    }
    let expected_name = format!("{}.json", journal.thread_id);
    if require_pending_name
        && path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
    {
        return Err("journal filename does not match its thread id".to_string());
    }
    if !require_pending_name {
        let path_hash = sha256_hex(
            &serde_json::to_vec(&journal.rollout_path)
                .map_err(|error| format!("encode journal path: {error}"))?,
        );
        let expected_name = format!("{}-{}.json", journal.thread_id, &path_hash[..16]);
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
            return Err("completed journal filename does not match its identity".to_string());
        }
    }
    if journal.original_token != format!("\"{}\"", journal.old_provider) {
        return Err("journal provider token is not canonical JSON".to_string());
    }
    if journal.header_len > MAX_HEADER_BYTES as usize
        || journal
            .offset
            .checked_add(journal.original_token.len())
            .is_none_or(|end| end > journal.header_len)
    {
        return Err("journal token span is invalid".to_string());
    }
    Ok(journal)
}

fn read_and_parse_rollout(rollout: &RolloutFile) -> Result<ParsedHeader, String> {
    let file = open_rollout(&rollout.canonical, false)?;
    let header = read_header(&file)?;
    parse_header(&header)
}

fn read_header(file: &File) -> Result<Vec<u8>, String> {
    let mut header = Vec::new();
    let mut offset = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let remaining = (MAX_HEADER_BYTES + 1).saturating_sub(offset);
        if remaining == 0 {
            return Err(format!(
                "session metadata first line exceeds the {} byte limit",
                MAX_HEADER_BYTES
            ));
        }
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = read_at(file, &mut buffer[..wanted], offset)
            .map_err(|error| format!("read rollout metadata: {error}"))?;
        if read == 0 {
            break;
        }
        if let Some(newline) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            header.extend_from_slice(&buffer[..=newline]);
            break;
        }
        header.extend_from_slice(&buffer[..read]);
        offset += read as u64;
    }
    if header.len() as u64 > MAX_HEADER_BYTES {
        return Err(format!(
            "session metadata first line exceeds the {} byte limit",
            MAX_HEADER_BYTES
        ));
    }
    if header.is_empty() {
        return Err("rollout metadata is empty".to_string());
    }
    Ok(header)
}

#[cfg(unix)]
fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<usize> {
    file.read_at(bytes, offset)
}

#[cfg(windows)]
fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<usize> {
    file.seek_read(bytes, offset)
}

fn parse_header(header: &[u8]) -> Result<ParsedHeader, String> {
    let envelope: HeaderEnvelope<'_> = serde_json::from_slice(header)
        .map_err(|error| format!("parse session metadata: {error}"))?;
    if envelope.kind.is_none()
        && envelope.payload.is_none()
        && envelope.id.is_some_and(valid_thread_id)
        && (envelope.timestamp.is_some() || envelope.instructions.is_some())
    {
        return Ok(ParsedHeader {
            thread_id: envelope.id.unwrap_or_default().to_string(),
            provider: None,
            token_offset: None,
            token: None,
        });
    }
    if envelope.kind != Some("session_meta") {
        return Err("first rollout record is not session_meta".to_string());
    }
    let payload = envelope
        .payload
        .ok_or_else(|| "session_meta record has no payload".to_string())?;
    let payload: SessionPayload<'_> = serde_json::from_str(payload.get())
        .map_err(|error| format!("parse session_meta payload: {error}"))?;
    let thread_id = payload
        .id
        .or(payload.session_id)
        .ok_or_else(|| "session_meta payload has no thread id".to_string())?;
    if !valid_thread_id(thread_id) {
        return Err("session_meta thread id is invalid".to_string());
    }
    let Some(raw_provider) = payload.model_provider else {
        return Ok(ParsedHeader {
            thread_id: thread_id.to_string(),
            provider: None,
            token_offset: None,
            token: None,
        });
    };
    let provider: String = serde_json::from_str(raw_provider.get())
        .map_err(|error| format!("model_provider is not a string: {error}"))?;
    let canonical_token = format!("\"{provider}\"");
    if raw_provider.get().as_bytes() != canonical_token.as_bytes() {
        return Err("model_provider is not a canonical quoted token".to_string());
    }
    let header_start = header.as_ptr() as usize;
    let token_start = raw_provider.get().as_ptr() as usize;
    let token_offset = token_start
        .checked_sub(header_start)
        .filter(|offset| {
            offset
                .checked_add(raw_provider.get().len())
                .is_some_and(|end| end <= header.len())
        })
        .ok_or_else(|| "model_provider token is outside its metadata line".to_string())?;
    Ok(ParsedHeader {
        thread_id: thread_id.to_string(),
        provider: Some(provider),
        token_offset: Some(token_offset),
        token: Some(raw_provider.get().as_bytes().to_vec()),
    })
}

fn valid_thread_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn is_legacy_provider(provider: &str) -> bool {
    if provider == "swapdex" {
        return true;
    }
    provider.strip_prefix("swapdex-").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    })
}

enum RepairFailure {
    Deferred(String),
    Error(String),
}

fn start_repair(
    paths: &Paths,
    rollout: &RolloutFile,
    scanned: &ParsedHeader,
    state: &mut StateDatabases,
) -> Result<RepairOutcome, RepairFailure> {
    let _writer_locks = acquire_writer_locks(&rollout.homes, &scanned.thread_id)?;
    let file = open_rollout(&rollout.canonical, true).map_err(RepairFailure::Error)?;
    verify_open_file_identity(&file, &rollout.metadata).map_err(RepairFailure::Error)?;
    let header = read_header(&file).map_err(RepairFailure::Error)?;
    let parsed = parse_header(&header).map_err(RepairFailure::Error)?;
    if parsed.thread_id != scanned.thread_id
        || parsed.provider != scanned.provider
        || parsed.token_offset != scanned.token_offset
        || parsed.token != scanned.token
    {
        return Err(RepairFailure::Error(
            "session metadata changed while the repair was being prepared".to_string(),
        ));
    }
    let old_provider = parsed
        .provider
        .as_deref()
        .ok_or_else(|| RepairFailure::Error("provider disappeared before repair".to_string()))?;
    let original_token = parsed.token.as_deref().ok_or_else(|| {
        RepairFailure::Error("provider token disappeared before repair".to_string())
    })?;
    let offset = parsed.token_offset.ok_or_else(|| {
        RepairFailure::Error("provider offset disappeared before repair".to_string())
    })?;
    let metadata = file.metadata().map_err(|error| {
        RepairFailure::Error(format!("inspect open rollout before repair: {error}"))
    })?;
    let (modified_secs, modified_nanos) = system_time_parts(rollout.metadata.modified().ok());
    let (accessed_secs, accessed_nanos) = system_time_parts(rollout.metadata.accessed().ok());
    let journal = Journal {
        version: JOURNAL_VERSION,
        rollout_path: rollout.canonical.clone(),
        thread_id: parsed.thread_id,
        old_provider: old_provider.to_string(),
        original_token: String::from_utf8(original_token.to_vec())
            .map_err(|error| RepairFailure::Error(format!("encode provider token: {error}")))?,
        offset,
        header_len: header.len(),
        original_len: metadata.len(),
        prefix_sha256: sha256_hex(&header[..offset]),
        suffix_sha256: sha256_hex(&header[offset + original_token.len()..]),
        device: metadata_device(&metadata),
        inode: metadata_inode(&metadata),
        modified_secs,
        modified_nanos,
        accessed_secs,
        accessed_nanos,
        aliases: rollout.aliases.clone(),
    };
    let journal_path = write_journal(paths, &journal).map_err(RepairFailure::Error)?;
    let header_changed = finish_open_repair(&file, &journal).map_err(RepairFailure::Error)?;
    Ok(finish_indexes(
        state,
        &journal,
        &journal_path,
        header_changed,
    ))
}

fn finish_journaled_repair(
    journal_path: &Path,
    journal: &Journal,
    rollout: &RolloutFile,
    state: &mut StateDatabases,
) -> Result<RepairOutcome, RepairFailure> {
    if rollout.canonical != journal.rollout_path {
        return Err(RepairFailure::Error(
            "journal path does not match the discovered rollout".to_string(),
        ));
    }
    let _writer_locks = acquire_writer_locks(&rollout.homes, &journal.thread_id)?;
    let file = open_rollout(&rollout.canonical, true).map_err(RepairFailure::Error)?;
    verify_journal_identity(&file, journal).map_err(RepairFailure::Error)?;
    let header_changed = finish_open_repair(&file, journal).map_err(RepairFailure::Error)?;
    Ok(finish_indexes(state, journal, journal_path, header_changed))
}

fn acquire_writer_locks(homes: &[PathBuf], thread_id: &str) -> Result<Vec<File>, RepairFailure> {
    let mut held = Vec::new();
    for home in homes {
        let directory = home.join(WRITER_LOCK_DIR);
        fs::create_dir_all(&directory).map_err(|error| {
            RepairFailure::Error(format!("create native writer lock directory: {error}"))
        })?;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
            RepairFailure::Error(format!("secure native writer lock directory: {error}"))
        })?;
        let coordination =
            open_lock_file(&directory.join(COORDINATION_LOCK)).map_err(RepairFailure::Error)?;
        if let Err(error) = coordination.try_lock_exclusive() {
            return if error.kind() == std::io::ErrorKind::WouldBlock {
                Err(RepairFailure::Deferred(
                    "native Codex writer-lock coordination is busy; retry later".to_string(),
                ))
            } else {
                Err(RepairFailure::Error(format!(
                    "acquire native writer-lock coordination: {error}"
                )))
            };
        }
        let writer_path = directory.join(format!("{thread_id}.lock"));
        let writer = open_lock_file(&writer_path).map_err(RepairFailure::Error)?;
        match writer.try_lock_exclusive() {
            Ok(()) => held.push(writer),
            Err(error) => {
                return if error.kind() == std::io::ErrorKind::WouldBlock {
                    Err(RepairFailure::Deferred(
                        "the Codex thread has an active writer; retry after it becomes idle"
                            .to_string(),
                    ))
                } else {
                    Err(RepairFailure::Error(format!(
                        "acquire native thread writer lock: {error}"
                    )))
                };
            }
        }
    }
    Ok(held)
}

fn open_lock_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .map_err(|error| format!("open native writer lock {}: {error}", path.display()))
}

fn open_rollout(path: &Path, writable: bool) -> Result<File, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect rollout before open: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("rollout leaf is not a regular file".to_string());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| format!("open rollout: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| format!("inspect open rollout: {error}"))?
        .is_file()
    {
        return Err("opened rollout is not a regular file".to_string());
    }
    Ok(file)
}

fn verify_open_file_identity(file: &File, expected: &fs::Metadata) -> Result<(), String> {
    let actual = file
        .metadata()
        .map_err(|error| format!("inspect open rollout identity: {error}"))?;
    #[cfg(unix)]
    if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err("rollout inode changed before repair".to_string());
    }
    Ok(())
}

fn verify_journal_identity(file: &File, journal: &Journal) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect journaled rollout: {error}"))?;
    #[cfg(unix)]
    if metadata.dev() != journal.device || metadata.ino() != journal.inode {
        return Err("journaled rollout inode no longer matches".to_string());
    }
    if metadata.len() < journal.original_len {
        return Err("journaled rollout was truncated".to_string());
    }
    Ok(())
}

fn write_journal(paths: &Paths, journal: &Journal) -> Result<PathBuf, String> {
    let directory = paths.store_dir().join(JOURNAL_DIR);
    fs::create_dir_all(&directory).map_err(|error| format!("create journal directory: {error}"))?;
    #[cfg(unix)]
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure journal directory: {error}"))?;
    let destination = directory.join(format!("{}.json", journal.thread_id));
    if destination.exists() {
        return Err("a repair journal already exists for this thread".to_string());
    }
    let temporary = directory.join(format!(".{}.{}.tmp", journal.thread_id, std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("create repair journal: {error}"))?;
    let write_result = (|| -> Result<(), String> {
        serde_json::to_writer(&mut file, journal)
            .map_err(|error| format!("serialize repair journal: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("finish repair journal: {error}"))?;
        file.flush()
            .map_err(|error| format!("flush repair journal: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync repair journal: {error}"))?;
        fs::rename(&temporary, &destination)
            .map_err(|error| format!("publish repair journal: {error}"))?;
        sync_directory(&directory)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.map(|()| destination)
}

fn finish_open_repair(file: &File, journal: &Journal) -> Result<bool, String> {
    let header = read_header(file)?;
    verify_journal_context(&header, journal)?;
    let original = journal.original_token.as_bytes();
    let replacement = replacement_token(original.len())?;
    let end = journal.offset + original.len();
    let current = &header[journal.offset..end];
    if !is_recoverable_token_state(current, original, &replacement) {
        return Err("provider token was replaced by unrelated content".to_string());
    }
    if current == replacement {
        return Ok(false);
    }
    write_all_at(file, &replacement, journal.offset as u64)
        .map_err(|error| format!("write provider token in place: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync repaired rollout: {error}"))?;
    let verified = read_header(file)?;
    verify_journal_context(&verified, journal)?;
    if verified[journal.offset..end] != replacement {
        return Err("provider token did not verify after repair".to_string());
    }
    if file
        .metadata()
        .map_err(|error| format!("inspect repaired rollout: {error}"))?
        .len()
        == journal.original_len
    {
        restore_file_times(file, journal)?;
    }
    Ok(true)
}

fn verify_journal_context(header: &[u8], journal: &Journal) -> Result<(), String> {
    if header.len() != journal.header_len {
        return Err("journaled rollout metadata length changed".to_string());
    }
    let end = journal.offset + journal.original_token.len();
    if sha256_hex(&header[..journal.offset]) != journal.prefix_sha256
        || sha256_hex(&header[end..]) != journal.suffix_sha256
    {
        return Err("journaled rollout metadata context changed".to_string());
    }
    Ok(())
}

fn replacement_token(width: usize) -> Result<Vec<u8>, String> {
    const OPENAI: &[u8] = b"\"openai\"";
    if width < OPENAI.len() {
        return Err("legacy provider token is shorter than the openai token".to_string());
    }
    let mut replacement = Vec::with_capacity(width);
    replacement.extend_from_slice(OPENAI);
    replacement.resize(width, b' ');
    Ok(replacement)
}

fn is_recoverable_token_state(current: &[u8], original: &[u8], replacement: &[u8]) -> bool {
    current.len() == original.len()
        && original.len() == replacement.len()
        && (0..=current.len()).any(|written| {
            current[..written] == replacement[..written]
                && current[written..] == original[written..]
        })
}

#[cfg(unix)]
fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    file.write_all_at(bytes, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write provider token",
            ));
        }
        bytes = &bytes[written..];
        offset += written as u64;
    }
    Ok(())
}

fn reconcile_stale_indexes(
    paths: &Paths,
    rollouts: &[(RolloutFile, ParsedHeader)],
    state: &mut StateDatabases,
    summary: &mut RepairSummary,
) {
    let mut stale = Vec::new();
    for (rollout, parsed) in rollouts {
        let (providers, errors) = stale_providers(state, rollout, &parsed.thread_id);
        for error in errors {
            summary.error(Some(&rollout.canonical), error);
        }
        if !providers.is_empty() {
            stale.push((rollout, parsed, providers));
        }
    }
    if stale.is_empty() {
        return;
    }

    let backups = discover_completed_journals(paths, summary);
    for (rollout, parsed, providers) in stale {
        for provider in providers {
            let candidates = backups.get(&parsed.thread_id).cloned().unwrap_or_default();
            let mut matching = None;
            for path in candidates {
                let Ok(journal) = read_journal_contents(&path, false) else {
                    continue;
                };
                if journal.old_provider != provider {
                    continue;
                }
                if completed_journal_matches(&journal, rollout, parsed).unwrap_or(false) {
                    matching = Some(journal);
                    break;
                }
            }
            let Some(mut journal) = matching else {
                summary.error(
                    Some(&rollout.canonical),
                    format!(
                        "state index reverted to {provider}, but no matching completed repair backup exists"
                    ),
                );
                continue;
            };
            let _writer_locks = match acquire_writer_locks(&rollout.homes, &parsed.thread_id) {
                Ok(locks) => locks,
                Err(RepairFailure::Deferred(message)) => {
                    summary.defer(Some(&rollout.canonical), message);
                    continue;
                }
                Err(RepairFailure::Error(message)) => {
                    summary.error(Some(&rollout.canonical), message);
                    continue;
                }
            };
            if let Err(error) = completed_journal_matches_after_lock(&journal, rollout, parsed) {
                summary.error(Some(&rollout.canonical), error);
                continue;
            }
            for alias in &rollout.aliases {
                if !journal.aliases.contains(alias) {
                    journal.aliases.push(alias.clone());
                }
            }
            let (rows, errors) = update_state_databases(state, &journal);
            summary.index_rows_repaired += rows;
            for error in errors {
                summary.error(Some(&rollout.canonical), error);
            }
        }
    }
}

fn stale_providers(
    state: &StateDatabases,
    rollout: &RolloutFile,
    thread_id: &str,
) -> (BTreeSet<String>, Vec<String>) {
    let mut providers = BTreeSet::new();
    let mut errors = Vec::new();
    for database in &state.compatible {
        for alias in &rollout.aliases {
            let result = database
                .connection
                .query_row(
                    "SELECT model_provider FROM threads WHERE id = ?1 AND rollout_path = ?2",
                    params![thread_id, alias.to_string_lossy()],
                    |row| row.get::<_, String>(0),
                )
                .optional();
            match result {
                Ok(Some(provider)) if is_legacy_provider(&provider) => {
                    providers.insert(provider);
                }
                Ok(_) => {}
                Err(error) => errors.push(format!(
                    "read state index {}: {error}",
                    database.path.display()
                )),
            }
        }
    }
    (providers, errors)
}

fn discover_completed_journals(
    paths: &Paths,
    summary: &mut RepairSummary,
) -> BTreeMap<String, Vec<PathBuf>> {
    let directory = paths.store_dir().join(JOURNAL_DIR).join("completed");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            summary.error(
                Some(&directory),
                format!("read completed repair backups: {error}"),
            );
            return BTreeMap::new();
        }
    };
    let mut backups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.len() < 38 || !name.ends_with(".json") {
            continue;
        }
        let thread_id = &name[..36];
        if !valid_thread_id(thread_id) || name.as_bytes().get(36) != Some(&b'-') {
            continue;
        }
        match entry.file_type() {
            Ok(file_type) if file_type.is_file() && !file_type.is_symlink() => {
                backups.entry(thread_id.to_string()).or_default().push(path)
            }
            Ok(_) => summary.error(Some(&path), "completed repair backup is not a regular file"),
            Err(error) => summary.error(
                Some(&path),
                format!("inspect completed repair backup: {error}"),
            ),
        }
    }
    backups
}

fn completed_journal_matches(
    journal: &Journal,
    rollout: &RolloutFile,
    parsed: &ParsedHeader,
) -> Result<bool, String> {
    if journal.thread_id != parsed.thread_id {
        return Ok(false);
    }
    let file = open_rollout(&rollout.canonical, false)?;
    if verify_journal_identity(&file, journal).is_err() {
        return Ok(false);
    }
    let header = read_header(&file)?;
    if verify_journal_context(&header, journal).is_err() {
        return Ok(false);
    }
    let replacement = replacement_token(journal.original_token.len())?;
    let end = journal.offset + journal.original_token.len();
    Ok(header[journal.offset..end] == replacement)
}

fn completed_journal_matches_after_lock(
    journal: &Journal,
    rollout: &RolloutFile,
    parsed: &ParsedHeader,
) -> Result<(), String> {
    if completed_journal_matches(journal, rollout, parsed)? {
        Ok(())
    } else {
        Err("completed repair backup no longer matches the rollout".to_string())
    }
}

fn finish_indexes(
    state: &mut StateDatabases,
    journal: &Journal,
    journal_path: &Path,
    header_changed: bool,
) -> RepairOutcome {
    let (index_rows, mut errors) = update_state_databases(state, journal);
    if errors.is_empty() {
        if let Err(error) = complete_journal(journal_path, journal) {
            errors.push(error);
        }
    }
    RepairOutcome {
        header_changed,
        index_rows,
        post_repair_error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

fn complete_journal(pending: &Path, journal: &Journal) -> Result<(), String> {
    let repair_directory = pending
        .parent()
        .ok_or_else(|| "repair journal has no parent directory".to_string())?;
    let completed = repair_directory.join("completed");
    fs::create_dir_all(&completed)
        .map_err(|error| format!("create completed journal directory: {error}"))?;
    #[cfg(unix)]
    fs::set_permissions(&completed, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure completed journal directory: {error}"))?;
    let path_hash = sha256_hex(
        &serde_json::to_vec(&journal.rollout_path)
            .map_err(|error| format!("encode journal path: {error}"))?,
    );
    let destination = completed.join(format!("{}-{}.json", journal.thread_id, &path_hash[..16]));
    if destination.exists() {
        let existing = read_journal_contents(&destination, false)?;
        if !journals_cover_same_token(&existing, journal) {
            return Err("completed journal key already holds different content".to_string());
        }
        fs::remove_file(pending)
            .map_err(|error| format!("remove duplicate pending journal: {error}"))?;
    } else {
        fs::rename(pending, &destination)
            .map_err(|error| format!("complete repair journal: {error}"))?;
    }
    sync_directory(repair_directory)?;
    sync_directory(&completed)
}

fn journals_cover_same_token(left: &Journal, right: &Journal) -> bool {
    left.rollout_path == right.rollout_path
        && left.thread_id == right.thread_id
        && left.old_provider == right.old_provider
        && left.original_token == right.original_token
        && left.offset == right.offset
        && left.header_len == right.header_len
        && left.prefix_sha256 == right.prefix_sha256
        && left.suffix_sha256 == right.suffix_sha256
        && left.device == right.device
        && left.inode == right.inode
}

fn update_state_databases(state: &mut StateDatabases, journal: &Journal) -> (usize, Vec<String>) {
    let mut updated = 0;
    let mut errors = Vec::new();
    let mut aliases = journal.aliases.clone();
    if !aliases.contains(&journal.rollout_path) {
        aliases.push(journal.rollout_path.clone());
    }
    for database in &mut state.compatible {
        let transaction = match database.connection.transaction() {
            Ok(transaction) => transaction,
            Err(error) => {
                errors.push(format!(
                    "begin state index update {}: {error}",
                    database.path.display()
                ));
                continue;
            }
        };
        let mut database_updated = 0;
        let mut database_error = None;
        for alias in &aliases {
            match transaction.execute(
                "UPDATE threads SET model_provider = 'openai'
                     WHERE id = ?1 AND rollout_path = ?2 AND model_provider = ?3",
                params![
                    &journal.thread_id,
                    alias.to_string_lossy(),
                    &journal.old_provider
                ],
            ) {
                Ok(rows) => database_updated += rows,
                Err(error) => {
                    database_error = Some(format!(
                        "update state index {}: {error}",
                        database.path.display()
                    ));
                    break;
                }
            }
        }
        if let Some(error) = database_error {
            errors.push(error);
            continue;
        }
        match transaction.commit() {
            Ok(()) => updated += database_updated,
            Err(error) => errors.push(format!(
                "commit state index update {}: {error}",
                database.path.display()
            )),
        }
    }
    (updated, errors)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync directory {}: {error}", path.display()))?;
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn system_time_parts(time: Option<SystemTime>) -> (Option<u64>, Option<u32>) {
    match time.and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        Some(duration) => (Some(duration.as_secs()), Some(duration.subsec_nanos())),
        None => (None, None),
    }
}

fn restore_file_times(file: &File, journal: &Journal) -> Result<(), String> {
    let mut times = FileTimes::new();
    if let (Some(seconds), Some(nanos)) = (journal.modified_secs, journal.modified_nanos) {
        times = times.set_modified(UNIX_EPOCH + Duration::new(seconds, nanos));
    }
    if let (Some(seconds), Some(nanos)) = (journal.accessed_secs, journal.accessed_nanos) {
        times = times.set_accessed(UNIX_EPOCH + Duration::new(seconds, nanos));
    }
    file.set_times(times)
        .map_err(|error| format!("restore rollout timestamps: {error}"))
}

#[cfg(unix)]
fn metadata_device(metadata: &fs::Metadata) -> u64 {
    metadata.dev()
}

#[cfg(not(unix))]
fn metadata_device(_metadata: &fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn metadata_inode(metadata: &fs::Metadata) -> u64 {
    metadata.ino()
}

#[cfg(not(unix))]
fn metadata_inode(_metadata: &fs::Metadata) -> u64 {
    0
}
