use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant};
use swapdex::paths::Paths;
use swapdex::quota_cache::{self, Entry};

const TOOL: &str = "codex";
const WRITER_WAIT: Duration = Duration::from_secs(5);

fn entry(percent: f64) -> Entry {
    Entry {
        five_h: Some(percent),
        at: 1_800_000_000,
        ..Default::default()
    }
}

fn cache_lock_path(paths: &Paths, tool: &str) -> PathBuf {
    let cache_name = match tool {
        "claude-code" => "quota-cache.json".to_string(),
        other => format!("{other}-quota-cache.json"),
    };
    paths.store_dir().join(format!(".{cache_name}.lock"))
}

fn hold_cache_lock(paths: &Paths, tool: &str) -> File {
    fs::create_dir_all(paths.store_dir()).unwrap();
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache_lock_path(paths, tool))
        .unwrap();
    file.lock_exclusive().unwrap();
    file
}

fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

#[test]
fn thread_writers_wait_for_the_cache_lock_and_preserve_both_updates() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let lock = hold_cache_lock(&paths, TOOL);
    let start = Arc::new(Barrier::new(3));
    let (done_tx, done_rx) = mpsc::channel();
    let mut writers = Vec::new();

    for (name, percent) in [("thread-a", 11.0), ("thread-b", 22.0)] {
        let paths = paths.clone();
        let start = Arc::clone(&start);
        let done_tx = done_tx.clone();
        writers.push(std::thread::spawn(move || {
            start.wait();
            quota_cache::update_for(&paths, TOOL, &[(name.to_string(), entry(percent))]);
            done_tx.send(name).unwrap();
        }));
    }
    drop(done_tx);

    start.wait();
    let finished_while_locked = done_rx.recv_timeout(Duration::from_millis(500)).ok();
    FileExt::unlock(&lock).unwrap();
    drop(lock);

    let mut completed: Vec<_> = finished_while_locked.iter().copied().collect();
    while completed.len() < 2 {
        completed.push(done_rx.recv_timeout(WRITER_WAIT).expect("writer finished"));
    }
    for writer in writers {
        writer.join().unwrap();
    }

    assert_eq!(
        finished_while_locked, None,
        "a cache writer ignored the transaction lock"
    );
    let cache = quota_cache::load_for(&paths, TOOL);
    assert_eq!(cache["thread-a"].five_h, Some(11.0));
    assert_eq!(cache["thread-b"].five_h, Some(22.0));
}

#[test]
fn simultaneous_distinct_account_updates_do_not_drop_observations() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let seed: Vec<_> = (0..12_000)
        .map(|index| (format!("seed-{index}"), entry(1.0)))
        .collect();
    quota_cache::update_for(&paths, TOOL, &seed);

    let start = Arc::new(Barrier::new(9));
    let mut writers = Vec::new();
    for index in 0..8 {
        let paths = paths.clone();
        let start = Arc::clone(&start);
        writers.push(std::thread::spawn(move || {
            start.wait();
            quota_cache::update_for(
                &paths,
                TOOL,
                &[(format!("simultaneous-{index}"), entry(10.0 + index as f64))],
            );
        }));
    }
    start.wait();
    for writer in writers {
        writer.join().unwrap();
    }

    let cache = quota_cache::load_for(&paths, TOOL);
    for index in 0..8 {
        assert!(
            cache.contains_key(&format!("simultaneous-{index}")),
            "concurrent update for account {index} was overwritten"
        );
    }
}

#[test]
fn a_concurrent_usage_read_and_traffic_reset_preserve_both_fields() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let mut seed: Vec<_> = (0..12_000)
        .map(|index| (format!("seed-{index}"), entry(1.0)))
        .collect();
    seed.push(("shared".to_string(), entry(5.0)));
    quota_cache::update_for(&paths, TOOL, &seed);

    let start = Arc::new(Barrier::new(3));
    let usage_paths = paths.clone();
    let usage_start = Arc::clone(&start);
    let usage = std::thread::spawn(move || {
        usage_start.wait();
        quota_cache::update_for(&usage_paths, TOOL, &[("shared".to_string(), entry(77.0))]);
    });
    let reset_paths = paths.clone();
    let reset_start = Arc::clone(&start);
    let reset = std::thread::spawn(move || {
        reset_start.wait();
        quota_cache::note_resets(&reset_paths, TOOL, "shared", Some(9_000_000_000), None);
    });

    start.wait();
    usage.join().unwrap();
    reset.join().unwrap();

    let shared = &quota_cache::load_for(&paths, TOOL)["shared"];
    assert_eq!(shared.five_h, Some(77.0), "the usage reading was dropped");
    assert_eq!(
        shared.five_h_reset,
        Some(9_000_000_000),
        "the traffic reset was dropped"
    );
}

#[test]
fn a_concurrent_reset_and_rejection_preserve_both_fields() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let mut seed: Vec<_> = (0..12_000)
        .map(|index| (format!("seed-{index}"), entry(1.0)))
        .collect();
    seed.push(("shared".to_string(), entry(5.0)));
    quota_cache::update_for(&paths, TOOL, &seed);

    let start = Arc::new(Barrier::new(3));
    let reset_paths = paths.clone();
    let reset_start = Arc::clone(&start);
    let reset = std::thread::spawn(move || {
        reset_start.wait();
        quota_cache::note_resets(&reset_paths, TOOL, "shared", Some(9_000_000_000), None);
    });
    let rejection_paths = paths.clone();
    let rejection_start = Arc::clone(&start);
    let rejection = std::thread::spawn(move || {
        rejection_start.wait();
        quota_cache::note_token_rejected(&rejection_paths, TOOL, "shared", 1_900_000_000);
    });

    start.wait();
    reset.join().unwrap();
    rejection.join().unwrap();

    let shared = &quota_cache::load_for(&paths, TOOL)["shared"];
    assert_eq!(
        shared.five_h,
        Some(5.0),
        "the remembered reading was dropped"
    );
    assert_eq!(shared.at, 1_800_000_000, "the reading was relabeled");
    assert_eq!(shared.five_h_reset, Some(9_000_000_000));
    assert_eq!(shared.token_rejected_at, Some(1_900_000_000));
}

#[test]
fn a_traffic_reset_survives_a_later_usage_read_that_omits_it() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    quota_cache::note_resets(&paths, TOOL, "shared", Some(9_000_000_000), None);

    quota_cache::update_for(&paths, TOOL, &[("shared".to_string(), entry(77.0))]);

    let shared = &quota_cache::load_for(&paths, TOOL)["shared"];
    assert_eq!(shared.five_h, Some(77.0));
    assert_eq!(shared.five_h_reset, Some(9_000_000_000));
    assert_eq!(shared.at, 1_800_000_000);
}

#[test]
fn locking_one_tool_does_not_block_another_tools_cache() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let lock = hold_cache_lock(&paths, TOOL);
    let writer_paths = paths.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        quota_cache::update_for(
            &writer_paths,
            "claude-code",
            &[("claude".to_string(), entry(55.0))],
        );
        done_tx.send(()).unwrap();
    });

    let completed_independently = done_rx.recv_timeout(WRITER_WAIT).is_ok();
    FileExt::unlock(&lock).unwrap();
    drop(lock);
    writer.join().unwrap();

    assert!(
        completed_independently,
        "the Codex cache lock stalled a Claude cache write"
    );
    assert_eq!(
        quota_cache::load_for(&paths, "claude-code")["claude"].five_h,
        Some(55.0)
    );
}

#[test]
fn cache_lock_files_are_private() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    quota_cache::update_for(&paths, TOOL, &[("work".to_string(), entry(10.0))]);

    let mode = fs::metadata(cache_lock_path(&paths, TOOL))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn an_unopenable_cache_lock_keeps_the_write_best_effort() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    fs::create_dir_all(cache_lock_path(&paths, TOOL)).unwrap();

    quota_cache::update_for(&paths, TOOL, &[("work".to_string(), entry(10.0))]);

    assert!(quota_cache::load_for(&paths, TOOL).is_empty());
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.as_mut().expect("child present").try_wait()
    }

    fn wait_bounded(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.try_wait() {
                Ok(Some(status)) => {
                    self.0.take();
                    return status.success();
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return false,
            }
        }
        false
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_process_writer(root: &Path, ready: &Path, name: &str, percent: f64) -> ChildGuard {
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("process_writer")
        .arg("--nocapture")
        .env("SWAPDEX_QUOTA_TEST_ROOT", root)
        .env("SWAPDEX_QUOTA_TEST_READY", ready)
        .env("SWAPDEX_QUOTA_TEST_NAME", name)
        .env("SWAPDEX_QUOTA_TEST_PERCENT", percent.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    ChildGuard(Some(child))
}

#[test]
fn process_writer() {
    let Ok(root) = std::env::var("SWAPDEX_QUOTA_TEST_ROOT") else {
        return;
    };
    let ready = PathBuf::from(std::env::var_os("SWAPDEX_QUOTA_TEST_READY").unwrap());
    let name = std::env::var("SWAPDEX_QUOTA_TEST_NAME").unwrap();
    let percent = std::env::var("SWAPDEX_QUOTA_TEST_PERCENT")
        .unwrap()
        .parse()
        .unwrap();
    fs::write(ready, b"ready").unwrap();
    quota_cache::update_for(
        &Paths::rooted(Path::new(&root)),
        TOOL,
        &[(name, entry(percent))],
    );
}

#[test]
fn separate_process_writers_wait_for_the_cache_lock_and_preserve_both_updates() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let lock = hold_cache_lock(&paths, TOOL);
    let ready_a = root.path().join("writer-a.ready");
    let ready_b = root.path().join("writer-b.ready");
    let mut writers = [
        spawn_process_writer(root.path(), &ready_a, "process-a", 33.0),
        spawn_process_writer(root.path(), &ready_b, "process-b", 44.0),
    ];

    let both_started = wait_for_path(&ready_a, WRITER_WAIT) && wait_for_path(&ready_b, WRITER_WAIT);
    let probe_deadline = Instant::now() + Duration::from_millis(500);
    let mut finished_while_locked = false;
    while both_started && Instant::now() < probe_deadline {
        finished_while_locked = writers
            .iter_mut()
            .any(|writer| writer.try_wait().unwrap().is_some());
        if finished_while_locked {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    FileExt::unlock(&lock).unwrap();
    drop(lock);
    let all_succeeded = writers
        .iter_mut()
        .all(|writer| writer.wait_bounded(WRITER_WAIT));

    assert!(both_started, "child writers did not reach the cache API");
    assert!(
        !finished_while_locked,
        "a process cache writer ignored the transaction lock"
    );
    assert!(all_succeeded, "a child cache writer failed or timed out");
    let cache = quota_cache::load_for(&paths, TOOL);
    assert_eq!(cache["process-a"].five_h, Some(33.0));
    assert_eq!(cache["process-b"].five_h, Some(44.0));
}
