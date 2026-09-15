use std::process::Command;

use swapdex::paths::Paths;
use swapdex::settings::{self, Settings};
use swapdex::store::Store;

fn seeded() -> (tempfile::TempDir, Paths, Vec<u8>) {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    settings::save(
        &paths,
        &Settings {
            proxy_auto: Some(true),
            proxy_threshold: Some(0.82),
            ..Default::default()
        },
    )
    .unwrap();
    let before = std::fs::read(paths.store_dir().join("settings.json")).unwrap();
    (root, paths, before)
}

#[test]
fn settings_contention_refuses_to_write_without_the_store_lock() {
    let (_root, paths, before) = seeded();
    let store = Store::open(&paths).unwrap();
    let guard = store.lock().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_swapdex"))
        .args(["threshold", "0.5"])
        .env("SWAPDEX_ROOT", paths.home())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(
        std::fs::read(paths.store_dir().join("settings.json")).unwrap(),
        before
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("settings") && stderr.contains("retry"),
        "{stderr}"
    );
    drop(guard);
    settings::update(&paths, |settings| settings.proxy_threshold = Some(0.5)).unwrap();
    let saved = settings::load(&paths);
    assert_eq!(saved.proxy_threshold, Some(0.5));
    assert_eq!(saved.proxy_auto, Some(true));
}

#[test]
fn settings_lock_io_failure_does_not_apply_the_edit() {
    let (_root, paths, before) = seeded();
    std::fs::create_dir(paths.store_dir().join(".lock")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_swapdex"))
        .args(["auto", "off"])
        .env("SWAPDEX_ROOT", paths.home())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(
        std::fs::read(paths.store_dir().join("settings.json")).unwrap(),
        before
    );
}
