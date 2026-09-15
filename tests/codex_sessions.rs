use fs2::FileExt as _;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use swapdex::codex_sessions::{repair_codex_sessions, RepairOptions};
use swapdex::paths::Paths;

const ID_ONE: &str = "11111111-1111-4111-8111-111111111111";
const ID_TWO: &str = "22222222-2222-4222-8222-222222222222";

fn rollout(home: &Path, archived: bool, id: &str, provider: &str, large: bool) -> PathBuf {
    let dir = if archived {
        home.join("archived_sessions")
    } else {
        home.join("sessions/2026/09/15")
    };
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-09-15T00-00-00-{id}.jsonl"));
    let padding = if large {
        "x".repeat(24 * 1024)
    } else {
        String::new()
    };
    let header = format!(
        "{{\"timestamp\":\"2026-09-15T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"large\":\"{padding}\",\"model_provider\":\"{provider}\",\"cwd\":\"/work\"}}}}\n"
    );
    let body = format!(
        "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"leave swapdex and openai here unchanged\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"id\":\"{id}\"}}}}\n"
    );
    fs::write(&path, format!("{header}{body}")).unwrap();
    path
}

fn first_line(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    let end = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    bytes[..end].to_vec()
}

fn provider(path: &Path) -> String {
    let line = first_line(path);
    let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
    value["payload"]["model_provider"]
        .as_str()
        .unwrap()
        .to_string()
}

fn create_state(home: &Path, name: &str, rows: &[(&str, &Path, &str)]) -> PathBuf {
    fs::create_dir_all(home).unwrap();
    let path = home.join(name);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads (
                 id TEXT PRIMARY KEY,
                 rollout_path TEXT NOT NULL,
                 model_provider TEXT NOT NULL,
                 title TEXT NOT NULL
             );
             CREATE TABLE untouched (value TEXT NOT NULL);
             INSERT INTO untouched VALUES ('sentinel');",
        )
        .unwrap();
    for (id, rollout_path, model_provider) in rows {
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider, title)
                 VALUES (?1, ?2, ?3, 'keep me')",
                params![id, rollout_path.to_string_lossy(), model_provider],
            )
            .unwrap();
    }
    path
}

fn db_provider(path: &Path, id: &str) -> String {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT model_provider FROM threads WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn completed_backups(paths: &Paths) -> Vec<PathBuf> {
    let directory = paths
        .store_dir()
        .join("codex-session-repair")
        .join("completed");
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    paths.sort();
    paths
}

#[test]
fn repairs_only_generated_provider_and_preserves_every_later_byte() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let legacy = rollout(paths.codex_dir(), false, ID_ONE, "swapdex-work_1", true);
    let unrelated = rollout(paths.codex_dir(), false, ID_TWO, "swapdex.example", false);
    let original = fs::read(&legacy).unwrap();
    let header_len = original.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let body = original[header_len..].to_vec();
    let original_unrelated = fs::read(&unrelated).unwrap();
    let original_mtime = fs::metadata(&legacy).unwrap().modified().unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 1, "{:#?}", result.issues);
    assert_eq!(result.errors, 0, "{:#?}", result.issues);
    assert_eq!(provider(&legacy), "openai");
    let repaired = fs::read(&legacy).unwrap();
    assert_eq!(repaired.len(), original.len());
    assert_eq!(&repaired[header_len..], body);
    assert_eq!(fs::read(&unrelated).unwrap(), original_unrelated);
    assert_eq!(
        fs::metadata(&legacy).unwrap().modified().unwrap(),
        original_mtime
    );

    let again = repair_codex_sessions(&paths, RepairOptions::default());
    assert_eq!(again.repaired, 0, "{:#?}", again.issues);
    assert_eq!(again.errors, 0, "{:#?}", again.issues);
    assert_eq!(fs::read(&legacy).unwrap(), repaired);
}

#[cfg(unix)]
#[test]
fn repairs_shared_rollout_once_and_each_existing_slot_index() {
    let root = tempfile::tempdir().unwrap();
    let active = root.path().join("active-codex");
    let registered = root.path().join("registered-codex");
    let shared = root.path().join("shared-sessions");
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&registered).unwrap();
    fs::create_dir_all(&shared).unwrap();
    std::os::unix::fs::symlink(&shared, active.join("sessions")).unwrap();
    std::os::unix::fs::symlink(&shared, registered.join("sessions")).unwrap();
    let paths = Paths::rooted(root.path()).with_tool_dir("codex", &active);
    fs::create_dir_all(paths.store_dir()).unwrap();
    fs::write(
        paths.store_dir().join("slots.json"),
        serde_json::to_vec(&serde_json::json!([{
            "name": "registered",
            "id": "slot-1",
            "config_dir": registered,
            "adopted": true,
            "tool": "codex"
        }]))
        .unwrap(),
    )
    .unwrap();
    let shared_rollout = rollout(&active, false, ID_ONE, "swapdex-team", false);
    let registered_path = registered.join("sessions").join(
        shared_rollout
            .strip_prefix(active.join("sessions"))
            .unwrap(),
    );
    let active_db = create_state(
        &active,
        "state_5.sqlite",
        &[(ID_ONE, &shared_rollout, "swapdex-team")],
    );
    let registered_db = create_state(
        &registered,
        "state_5.sqlite",
        &[(ID_ONE, &registered_path, "swapdex-team")],
    );
    let mismatched_provider_db = create_state(
        &registered,
        "state_6.sqlite",
        &[(ID_ONE, &registered_path, "swapdex-other")],
    );

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 1, "{:#?}", result.issues);
    assert_eq!(result.index_rows_repaired, 2, "{:#?}", result.issues);
    assert_eq!(db_provider(&active_db, ID_ONE), "openai");
    assert_eq!(db_provider(&registered_db, ID_ONE), "openai");
    assert_eq!(
        db_provider(&mismatched_provider_db, ID_ONE),
        "swapdex-other"
    );
    assert_eq!(
        Connection::open(active_db)
            .unwrap()
            .query_row("SELECT value FROM untouched", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "sentinel"
    );
    assert_eq!(
        Connection::open(registered_db)
            .unwrap()
            .query_row("SELECT title FROM threads WHERE id = ?1", [ID_ONE], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "keep me"
    );
}

#[test]
fn repairs_archived_rollouts_and_the_bare_home_when_active_home_differs() {
    let root = tempfile::tempdir().unwrap();
    let active = root.path().join("active-codex");
    fs::create_dir_all(&active).unwrap();
    let paths = Paths::rooted(root.path()).with_tool_dir("codex", &active);
    let archived = rollout(&root.path().join(".codex"), true, ID_ONE, "swapdex", false);

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 1, "{:#?}", result.issues);
    assert_eq!(provider(&archived), "openai");
}

#[test]
fn dry_run_reports_candidate_without_creating_any_lock_or_journal() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let file = rollout(paths.codex_dir(), false, ID_ONE, "swapdex-personal", false);
    let original = fs::read(&file).unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions { dry_run: true });

    assert_eq!(result.eligible, 1, "{:#?}", result.issues);
    assert_eq!(result.repaired, 0);
    assert_eq!(result.errors, 0, "{:#?}", result.issues);
    assert_eq!(fs::read(file).unwrap(), original);
    assert!(!paths.store_dir().exists());
    assert!(!paths.codex_dir().join("thread-writer-locks").exists());
}

#[test]
fn malformed_metadata_and_unknown_state_schema_are_reported_without_db_creation() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let sessions = paths.codex_dir().join("sessions/2026/09/15");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("rollout-2026-09-15-{ID_ONE}.jsonl")),
        b"{not valid json}\nbody\n",
    )
    .unwrap();
    fs::write(paths.codex_dir().join("state_5.sqlite"), b"not sqlite").unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert!(result.errors >= 2, "{:#?}", result.issues);
    assert!(!paths.codex_dir().join("state_6.sqlite").exists());
}

#[test]
fn recognized_flat_legacy_codex_metadata_is_skipped_without_an_error() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let directory = paths.codex_dir().join("sessions/2025/01/01");
    fs::create_dir_all(&directory).unwrap();
    let file = directory.join(format!("rollout-2025-01-01-{ID_ONE}.jsonl"));
    let bytes = format!(
        "{{\"id\":\"{ID_ONE}\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"instructions\":\"old format\"}}\n{{\"history\":true}}\n"
    )
    .into_bytes();
    fs::write(&file, &bytes).unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.errors, 0, "{:#?}", result.issues);
    assert_eq!(result.skipped, 1);
    assert_eq!(fs::read(file).unwrap(), bytes);
}

#[test]
fn a_newline_just_beyond_the_header_limit_is_rejected_before_journaling() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let directory = paths.codex_dir().join("sessions/2026/09/15");
    fs::create_dir_all(&directory).unwrap();
    let file = directory.join(format!("rollout-2026-09-15-{ID_ONE}.jsonl"));
    let mut bytes = vec![b' '; 1024 * 1024];
    bytes.push(b'\n');
    fs::write(&file, bytes).unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 0);
    assert_eq!(result.errors, 1, "{:#?}", result.issues);
    assert!(result.issues[0].message.contains("exceeds"));
    assert!(completed_backups(&paths).is_empty());
}

#[test]
fn an_index_failure_keeps_the_completed_rollout_journal_for_retry() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let file = rollout(paths.codex_dir(), false, ID_ONE, "swapdex-work", false);
    let good = create_state(
        paths.codex_dir(),
        "state_5.sqlite",
        &[(ID_ONE, &file, "swapdex-work")],
    );
    let bad = paths.codex_dir().join("state_unknown.sqlite");
    Connection::open(&bad)
        .unwrap()
        .execute("CREATE TABLE something_else (value TEXT)", [])
        .unwrap();

    let first = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(first.repaired, 1, "{:#?}", first.issues);
    assert_eq!(first.errors, 1, "{:#?}", first.issues);
    assert_eq!(provider(&file), "openai");
    assert_eq!(db_provider(&good, ID_ONE), "openai");
    assert_eq!(completed_backups(&paths).len(), 1);

    fs::remove_file(bad).unwrap();
    let retry = repair_codex_sessions(&paths, RepairOptions::default());
    assert_eq!(retry.repaired, 0, "{:#?}", retry.issues);
    assert_eq!(retry.errors, 0, "{:#?}", retry.issues);
    assert_eq!(completed_backups(&paths).len(), 1);

    Connection::open(&good)
        .unwrap()
        .execute(
            "UPDATE threads SET model_provider = 'swapdex-work' WHERE id = ?1",
            [ID_ONE],
        )
        .unwrap();
    let stale_retry = repair_codex_sessions(&paths, RepairOptions::default());
    assert_eq!(stale_retry.repaired, 0, "{:#?}", stale_retry.issues);
    assert_eq!(
        stale_retry.index_rows_repaired, 1,
        "{:#?}",
        stale_retry.issues
    );
    assert_eq!(db_provider(&good, ID_ONE), "openai");
    assert_eq!(completed_backups(&paths).len(), 1);
}

#[test]
fn dry_run_does_not_create_wal_sidecars() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    rollout(paths.codex_dir(), false, ID_ONE, "swapdex-work", false);
    let database = create_state(paths.codex_dir(), "state_5.sqlite", &[]);
    {
        let connection = Connection::open(&database).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    let wal = database.with_file_name("state_5.sqlite-wal");
    let shm = database.with_file_name("state_5.sqlite-shm");
    let _ = fs::remove_file(&wal);
    let _ = fs::remove_file(&shm);

    let result = repair_codex_sessions(&paths, RepairOptions { dry_run: true });

    assert_eq!(result.eligible, 1, "{:#?}", result.issues);
    assert!(!wal.exists());
    assert!(!shm.exists());
}

#[test]
fn moving_a_repaired_rollout_to_the_archive_does_not_make_its_backup_an_error() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let file = rollout(paths.codex_dir(), false, ID_ONE, "swapdex", false);
    let first = repair_codex_sessions(&paths, RepairOptions::default());
    assert_eq!(first.repaired, 1, "{:#?}", first.issues);
    let archived = paths
        .codex_dir()
        .join("archived_sessions")
        .join(file.file_name().unwrap());
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(file, &archived).unwrap();

    let again = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(again.repaired, 0, "{:#?}", again.issues);
    assert_eq!(again.errors, 0, "{:#?}", again.issues);
    assert_eq!(provider(&archived), "openai");
    assert_eq!(completed_backups(&paths).len(), 1);
}

#[test]
fn independent_rollouts_with_the_same_thread_id_keep_distinct_backups() {
    let root = tempfile::tempdir().unwrap();
    let active = root.path().join("active-codex");
    fs::create_dir_all(&active).unwrap();
    let paths = Paths::rooted(root.path()).with_tool_dir("codex", &active);
    let first = rollout(&active, false, ID_ONE, "swapdex-one", false);
    let second = rollout(
        &root.path().join(".codex"),
        false,
        ID_ONE,
        "swapdex-two",
        false,
    );

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 2, "{:#?}", result.issues);
    assert_eq!(result.errors, 0, "{:#?}", result.issues);
    assert_eq!(provider(&first), "openai");
    assert_eq!(provider(&second), "openai");
    assert_eq!(completed_backups(&paths).len(), 2);
}

#[cfg(unix)]
#[test]
fn preserves_inode_and_an_append_handle_opened_before_repair() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let file = rollout(paths.codex_dir(), false, ID_ONE, "swapdex-work", false);
    let inode = fs::metadata(&file).unwrap().ino();
    let mut append = OpenOptions::new().append(true).open(&file).unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());
    append.write_all(b"{\"later\":true}\n").unwrap();
    append.flush().unwrap();

    assert_eq!(result.errors, 0, "{:#?}", result.issues);
    assert_eq!(fs::metadata(&file).unwrap().ino(), inode);
    let bytes = fs::read(&file).unwrap();
    assert!(bytes.ends_with(b"{\"later\":true}\n"));
    assert_eq!(provider(&file), "openai");
}

#[test]
fn a_busy_native_writer_lock_leaves_the_rollout_retryable() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let file = rollout(paths.codex_dir(), false, ID_ONE, "swapdex-work", false);
    let original = fs::read(&file).unwrap();
    let locks = paths.codex_dir().join("thread-writer-locks");
    fs::create_dir_all(&locks).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(locks.join(format!("{ID_ONE}.lock")))
        .unwrap();
    lock.try_lock_exclusive().unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 0, "{:#?}", result.issues);
    assert_eq!(result.deferred, 1, "{:#?}", result.issues);
    assert_eq!(result.errors, 0, "{:#?}", result.issues);
    assert_eq!(fs::read(file).unwrap(), original);
    assert!(!paths
        .store_dir()
        .join("codex-session-repair")
        .join(format!("{ID_ONE}.json"))
        .exists());
}

#[test]
fn compressed_rollouts_are_skipped_with_a_warning() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let dir = paths.codex_dir().join("sessions/2026/09/15");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("rollout-2026-09-15-{ID_ONE}.jsonl.zst")),
        b"compressed bytes",
    )
    .unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 0);
    assert_eq!(result.skipped, 1);
    assert_eq!(result.warnings, 1, "{:#?}", result.issues);
    assert_eq!(result.errors, 0, "{:#?}", result.issues);
}

#[cfg(unix)]
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn prepare_partial_journal(
    paths: &Paths,
    file: &Path,
    id: &str,
    old_provider: &str,
) -> (PathBuf, usize) {
    use std::os::unix::fs::FileExt;

    let header = first_line(file);
    let original_token = format!("\"{old_provider}\"");
    let offset = header
        .windows(original_token.len())
        .position(|window| window == original_token.as_bytes())
        .unwrap();
    let metadata = fs::metadata(file).unwrap();
    let canonical = fs::canonicalize(file).unwrap();
    let journal_dir = paths.store_dir().join("codex-session-repair");
    fs::create_dir_all(&journal_dir).unwrap();
    fs::set_permissions(&journal_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let journal = serde_json::json!({
        "version": 1,
        "rollout_path": canonical,
        "thread_id": id,
        "old_provider": old_provider,
        "original_token": original_token,
        "offset": offset,
        "header_len": header.len(),
        "original_len": metadata.len(),
        "prefix_sha256": sha256_hex(&header[..offset]),
        "suffix_sha256": sha256_hex(&header[offset + original_token.len()..]),
        "device": metadata.dev(),
        "inode": metadata.ino(),
        "aliases": [file],
    });
    let journal_path = journal_dir.join(format!("{id}.json"));
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).unwrap();

    let replacement = format!(
        "\"openai\"{}",
        " ".repeat(original_token.len() - "\"openai\"".len())
    );
    let open = OpenOptions::new()
        .read(true)
        .write(true)
        .open(file)
        .unwrap();
    open.write_all_at(&replacement.as_bytes()[..4], offset as u64)
        .unwrap();
    (journal_path, offset)
}

#[cfg(unix)]
#[test]
fn completes_a_journaled_partial_token_then_updates_the_index() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let file = rollout(paths.codex_dir(), false, ID_ONE, "swapdex-work", false);
    let body_before = {
        let bytes = fs::read(&file).unwrap();
        let split = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        bytes[split..].to_vec()
    };
    let db = create_state(
        paths.codex_dir(),
        "state_5.sqlite",
        &[(ID_ONE, &file, "swapdex-work")],
    );
    let (journal, _) = prepare_partial_journal(&paths, &file, ID_ONE, "swapdex-work");

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 1, "{:#?}", result.issues);
    assert_eq!(result.errors, 0, "{:#?}", result.issues);
    assert_eq!(provider(&file), "openai");
    assert_eq!(db_provider(&db, ID_ONE), "openai");
    let bytes = fs::read(&file).unwrap();
    let split = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    assert_eq!(&bytes[split..], body_before);
    assert!(!journal.exists());
    assert_eq!(completed_backups(&paths).len(), 1);
}

#[cfg(unix)]
#[test]
fn recovery_refuses_an_unrelated_same_length_token() {
    use std::os::unix::fs::FileExt;

    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let file = rollout(paths.codex_dir(), false, ID_ONE, "swapdex-work", false);
    let (journal, offset) = prepare_partial_journal(&paths, &file, ID_ONE, "swapdex-work");
    let open = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    open.write_all_at(b"\"other-model!\"", offset as u64)
        .unwrap();

    let result = repair_codex_sessions(&paths, RepairOptions::default());

    assert_eq!(result.repaired, 0, "{:#?}", result.issues);
    assert_eq!(result.errors, 1, "{:#?}", result.issues);
    assert!(journal.exists());
    assert_eq!(provider(&file), "other-model!");
}

#[test]
fn quiet_cli_has_no_success_output() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    rollout(paths.codex_dir(), false, ID_ONE, "swapdex", false);

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_swapdex"))
        .args(["repair-codex-sessions", "--quiet"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn command_errors_exit_nonzero_even_when_quiet() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let dir = paths.codex_dir().join("sessions/2026/09/15");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("rollout-2026-09-15-{ID_ONE}.jsonl")),
        b"bad\n",
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_swapdex"))
        .args(["repair-codex-sessions", "--quiet"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}
