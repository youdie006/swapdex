//! First-use behavior with disposable homes and fake native clients only.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn fake_tool(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(dirs: &[&Path]) -> String {
    let mut entries: Vec<String> = dirs
        .iter()
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect();
    entries.extend(["/usr/bin".into(), "/bin".into()]);
    entries.join(":")
}

fn run(root: &Path, args: &[&str], path: &str, input: Option<&str>) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(bin());
    command
        .args(args)
        .env_clear()
        .env("SWAPDEX_ROOT", root)
        .env("HOME", &home)
        .env("PATH", path)
        .env("SHELL", "/bin/sh")
        .env("USER", "swapdex-fixture")
        .env("LOGNAME", "swapdex-fixture")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(input) = input {
        command.env("SWAPDEX_ASSUME_TTY", "1").stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    } else {
        command.stdin(Stdio::null()).output().unwrap()
    }
}

fn shim_dir(root: &Path) -> PathBuf {
    root.join(".local/share/swapdex/bin")
}

#[test]
fn shim_installs_each_available_client_independently() {
    for (claude, codex, expected_status) in [
        (true, false, 0),
        (false, true, 0),
        (true, true, 0),
        (false, false, 1),
    ] {
        let root = tempfile::tempdir().unwrap();
        let native = root.path().join("native");
        if claude {
            fake_tool(&native, "claude");
        }
        if codex {
            fake_tool(&native, "codex");
        }
        let output = run(root.path(), &["shim"], &path_with(&[&native]), None);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(expected_status),
            "claude={claude} codex={codex}: stdout={stdout:?} stderr={stderr:?}"
        );
        assert_eq!(shim_dir(root.path()).join("claude").exists(), claude);
        assert_eq!(shim_dir(root.path()).join("codex").exists(), codex);
        if claude {
            assert!(stdout.contains("installed the claude shim"), "{stdout}");
        }
        if codex {
            assert!(stdout.contains("installed the codex shim"), "{stdout}");
        }
        if !claude && codex {
            assert!(stdout.contains("no `claude` on PATH"), "{stdout}");
            assert!(!root.path().join(".claude/settings.json").exists());
        }
        if !claude && !codex {
            assert!(stderr.contains("install Claude Code or Codex"), "{stderr}");
            assert!(!root.path().join(".zshrc").exists());
            assert!(!root.path().join(".claude/settings.json").exists());
        }
    }
}

#[test]
fn shim_reports_path_precedence_for_each_installed_tool() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    let earlier = root.path().join("earlier");
    fake_tool(&native, "claude");
    fake_tool(&native, "codex");
    fake_tool(&earlier, "codex");
    let installed = shim_dir(root.path());
    let path = path_with(&[&earlier, &installed, &native]);

    let output = run(root.path(), &["shim"], &path, None);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("plain `claude` goes through its shim"),
        "{stdout}"
    );
    assert!(
        stdout.contains("holds `codex`") && stdout.contains("codex shim never runs"),
        "{stdout}"
    );
}

#[test]
fn onboard_offers_only_available_missing_shims() {
    let codex_only = tempfile::tempdir().unwrap();
    let codex_native = codex_only.path().join("native");
    fake_tool(&codex_native, "codex");
    let accepted = run(
        codex_only.path(),
        &["onboard"],
        &path_with(&[&codex_native]),
        Some("y\n"),
    );
    let accepted_stdout = String::from_utf8_lossy(&accepted.stdout);
    assert!(accepted.status.success(), "{accepted_stdout}");
    assert!(
        accepted_stdout.contains("plain `codex`"),
        "{accepted_stdout}"
    );
    assert!(
        !accepted_stdout.contains("plain `claude`"),
        "{accepted_stdout}"
    );
    assert!(
        !accepted_stdout.contains("installed the claude shim"),
        "{accepted_stdout}"
    );
    assert!(shim_dir(codex_only.path()).join("codex").is_file());

    let empty = tempfile::tempdir().unwrap();
    let empty_native = empty.path().join("native");
    std::fs::create_dir_all(&empty_native).unwrap();
    let empty_output = run(
        empty.path(),
        &["onboard"],
        &path_with(&[&empty_native]),
        Some("y\n"),
    );
    let empty_stdout = String::from_utf8_lossy(&empty_output.stdout);
    assert!(empty_output.status.success(), "{empty_stdout}");
    assert!(
        !empty_stdout.contains("installs a small shim"),
        "{empty_stdout}"
    );
    assert!(!shim_dir(empty.path()).exists());
}

#[test]
fn onboard_respects_declined_and_noninteractive_shim_offers() {
    let declined = tempfile::tempdir().unwrap();
    let declined_native = declined.path().join("native");
    fake_tool(&declined_native, "codex");
    let declined_output = run(
        declined.path(),
        &["onboard"],
        &path_with(&[&declined_native]),
        Some("n\n"),
    );
    assert!(declined_output.status.success());
    assert!(!shim_dir(declined.path()).join("codex").exists());

    let noninteractive = tempfile::tempdir().unwrap();
    let noninteractive_native = noninteractive.path().join("native");
    fake_tool(&noninteractive_native, "codex");
    let noninteractive_output = run(
        noninteractive.path(),
        &["onboard"],
        &path_with(&[&noninteractive_native]),
        None,
    );
    let stdout = String::from_utf8_lossy(&noninteractive_output.stdout);
    assert!(noninteractive_output.status.success(), "{stdout}");
    assert!(!stdout.contains("installs a small shim"), "{stdout}");
    assert!(!shim_dir(noninteractive.path()).join("codex").exists());
}

#[test]
fn onboard_offers_codex_after_the_claude_shim_already_exists() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fake_tool(&native, "claude");
    let path = path_with(&[&native]);
    let installed = run(root.path(), &["shim"], &path, None);
    assert!(installed.status.success());
    assert!(shim_dir(root.path()).join("claude").is_file());
    assert!(!shim_dir(root.path()).join("codex").exists());

    fake_tool(&native, "codex");
    let onboarded = run(root.path(), &["onboard"], &path, Some("y\n"));
    let stdout = String::from_utf8_lossy(&onboarded.stdout);

    assert!(onboarded.status.success(), "{stdout}");
    assert!(stdout.contains("plain `codex`"), "{stdout}");
    assert!(!stdout.contains("plain `claude`"), "{stdout}");
    assert!(!stdout.contains("installed the claude shim"), "{stdout}");
    assert!(shim_dir(root.path()).join("codex").is_file());
}

#[test]
fn setup_explains_how_a_direct_session_becomes_managed() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fake_tool(&native, "codex");
    let path = path_with(&[&native]);
    for name in ["work", "personal"] {
        let created = run(
            root.path(),
            &["run", name, "--tool", "codex", "--no-launch"],
            &path,
            None,
        );
        assert!(created.status.success());
    }

    let setup = run(root.path(), &["setup"], &path, Some("n\n"));
    let stdout = String::from_utf8_lossy(&setup.stdout);

    assert!(setup.status.success(), "{stdout}");
    assert!(
        !stdout.contains("Switching takes effect on your next message - no restart needed"),
        "{stdout}"
    );
    assert!(stdout.contains("swapdex shim"), "{stdout}");
    assert!(stdout.contains("managed session"), "{stdout}");
    assert!(stdout.contains("relaunch"), "{stdout}");
    assert!(stdout.contains("--tool codex"), "{stdout}");
}

#[test]
fn codex_serve_hint_retains_its_tool_selector() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fake_tool(&native, "codex");
    let path = path_with(&[&native]);
    let created = run(
        root.path(),
        &["run", "work", "--tool", "codex", "--no-launch"],
        &path,
        None,
    );
    assert!(created.status.success());

    let output = run(root.path(), &["serve", "--tool", "codex"], &path, None);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("swapdex serve <name> --tool codex"),
        "{stdout}"
    );
}
