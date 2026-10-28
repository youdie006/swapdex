//! `doctor` names a Claude account whose copy of a shared file is its own file:
//! an edit to ~/.claude then never reaches that account's sessions.

use std::os::unix::fs::symlink;
use std::process::Command;
use swapdex::paths::Paths;

#[test]
fn doctor_names_each_account_keeping_its_own_settings() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    std::fs::create_dir_all(paths.claude_dir()).unwrap();
    std::fs::write(paths.claude_dir().join("settings.json"), "{}").unwrap();
    let mut slots = Vec::new();
    for name in ["bsgong", "kong"] {
        let slot = swapdex::slots::Slots::open_for_update(&paths, "claude-code")
            .unwrap()
            .create(name)
            .unwrap()
            .config_dir;
        std::fs::write(slot.join("settings.json"), "{}").unwrap();
        slots.push(slot);
    }
    let doctor = || {
        let out = Command::new(env!("CARGO_BIN_EXE_swapdex"))
            .arg("doctor")
            .env("SWAPDEX_ROOT", root.path())
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.lines()
            .find(|line| line.starts_with("sharing "))
            .unwrap_or_else(|| panic!("doctor must have a sharing row: {text}"))
            .to_string()
    };

    let row = doctor();
    for expected in [
        "problem",
        "bsgong",
        "kong",
        "settings.json",
        "edit these too",
    ] {
        assert!(
            row.contains(expected),
            "sharing row must contain {expected}: {row}"
        );
    }

    for slot in &slots {
        std::fs::remove_file(slot.join("settings.json")).unwrap();
        symlink(
            paths.claude_dir().join("settings.json"),
            slot.join("settings.json"),
        )
        .unwrap();
    }
    let row = doctor();
    assert!(row.contains(" ok -"), "every copy is a link again: {row}");
}
