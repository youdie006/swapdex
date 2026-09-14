//! Provider scoping at the full-screen picker boundary.
//!
//! A profile name is unique within a tool, so the picker must carry the tool
//! from the row the user selected all the way into the command it runs.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn fixture() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let store = t.path().join(".local/share/swapdex");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("onboarded"), b"1").unwrap();

    // The dashboard starts a quota read after its first frame. Keep this test
    // hermetic: the fixture curl makes every probe fail locally and immediately.
    let curl = t.path().join("fake-curl");
    std::fs::write(&curl, b"#!/bin/sh\nexit 22\n").unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
    t
}

fn codex_id_token(email: &str) -> String {
    use base64::Engine;
    let body = serde_json::json!({"email": email}).to_string();
    format!(
        "h.{}.s",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body)
    )
}

fn seed_claude_snapshot(root: &Path, name: &str, email: &str) {
    let dir = root
        .join(".local/share/swapdex/accounts")
        .join(name)
        .join("claude-code");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("credentials"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "AT",
                "refreshToken": "RT",
                "expiresAt": 9999999999999i64,
                "subscriptionType": "max"
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("oauth_account"),
        serde_json::json!({
            "accountUuid": "claude-account",
            "emailAddress": email
        })
        .to_string(),
    )
    .unwrap();
}

fn seed_codex_snapshot(root: &Path, name: &str, email: &str) {
    let dir = root
        .join(".local/share/swapdex/accounts")
        .join(name)
        .join("codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "last_refresh": "2026-09-14T00:00:00Z",
            "tokens": {
                "id_token": codex_id_token(email),
                "access_token": "AT",
                "refresh_token": "RT",
                "account_id": "codex-account"
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn seed_codex_slot(root: &Path, name: &str, email: &str) -> PathBuf {
    let dir = root.join(".local/share/swapdex/codex-slots").join(name);
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        serde_json::json!({
            "auth_mode": "chatgpt",
            "last_refresh": "2026-09-14T00:00:00Z",
            "tokens": {
                "id_token": codex_id_token(email),
                "access_token": "AT",
                "refresh_token": "RT",
                "account_id": "codex-slot-account"
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join(".local/share/swapdex/slots.json"),
        serde_json::to_vec_pretty(&vec![serde_json::json!({
            "name": name,
            "id": name,
            "config_dir": dir,
            "adopted": false,
            "tool": "codex"
        })])
        .unwrap(),
    )
    .unwrap();
    dir
}

fn seed_empty_claude_slot(root: &Path, name: &str) {
    let dir = root.join(".local/share/swapdex/claude-slots").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        root.join(".local/share/swapdex/slots.json"),
        serde_json::to_vec_pretty(&vec![serde_json::json!({
            "name": name,
            "id": name,
            "config_dir": dir,
            "adopted": false,
            "tool": "claude-code"
        })])
        .unwrap(),
    )
    .unwrap();
}

/// Drive the real alternate-screen UI. Each tuple waits, then writes its keys;
/// the delay lets destructive confirmation distinguish typed input from paste.
fn run_fullscreen_ui(root: &Path, input: &[(u64, &[u8])]) -> (String, i32) {
    run_fullscreen_ui_after_screen(root, input, None)
}

/// Drive the UI, optionally waiting for rendered text before sending a final
/// key sequence. The rendered prompt is the readiness signal for interactions
/// whose preceding key runs a blocking child command.
fn run_fullscreen_ui_after_screen(
    root: &Path,
    input: &[(u64, &[u8])],
    followup: Option<(&str, u64, &[u8])>,
) -> (String, i32) {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };

    let mut command = Command::new(bin());
    command
        .arg("ui")
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_CURL", root.join("fake-curl"))
        .env("TERM", "xterm")
        .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(command);

    unsafe {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
        let flags = libc::fcntl(fd, libc::F_GETFL);
        assert_ne!(flags, -1, "F_GETFL on pty master");
        assert_ne!(
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK),
            -1,
            "F_SETFL O_NONBLOCK on pty master"
        );
    }

    let mut writer = master.try_clone().unwrap();
    let input: Vec<(u64, Vec<u8>)> = input
        .iter()
        .map(|(delay, keys)| (*delay, keys.to_vec()))
        .collect();
    let followup =
        followup.map(|(text, delay, keys)| (text.as_bytes().to_vec(), delay, keys.to_vec()));
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = std::sync::Arc::clone(&seen);
    let writer = std::thread::spawn(move || {
        let wait_for_text = |text: &[u8]| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !observed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .windows(text.len())
                .any(|window| window == text)
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for UI text: {}",
                    String::from_utf8_lossy(text)
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        // A fixed startup sleep raced the picker under parallel test load. Its
        // key-hint footer means the first frame is complete and input is live.
        wait_for_text(b"switch by number");
        for (delay, keys) in input {
            std::thread::sleep(std::time::Duration::from_millis(delay));
            writer.write_all(&keys).unwrap();
        }
        if let Some((text, delay, keys)) = followup {
            wait_for_text(&text);
            std::thread::sleep(std::time::Duration::from_millis(delay));
            writer.write_all(&keys).unwrap();
        }
    });

    let drain = |master: &mut std::fs::File, seen: &std::sync::Mutex<Vec<u8>>| {
        let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = [0u8; 4096];
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        drain(&mut master, &seen);
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            panic!(
                "full-screen UI timed out:\n{}",
                String::from_utf8_lossy(&seen)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    writer.join().unwrap();
    drain(&mut master, &seen);
    let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
    (
        format!(
            "{}\n[child exit status: {status}]",
            String::from_utf8_lossy(&seen)
        ),
        status.code().unwrap_or(-1),
    )
}

#[test]
fn same_name_snapshot_is_one_row_per_provider() {
    let t = fixture();
    seed_claude_snapshot(t.path(), "shared", "claude@example.com");
    seed_codex_snapshot(t.path(), "shared", "codex@example.com");

    let (screen, code) = run_fullscreen_ui(t.path(), &[(300, b"q")]);
    assert_eq!(code, 0, "UI failed:\n{screen}");
    assert!(screen.contains("claude@example.com"), "{screen}");
    assert!(
        screen.contains("codex@example.com"),
        "the Codex half of a same-named profile had no independent row:\n{screen}"
    );
}

#[test]
fn codex_snapshot_does_not_borrow_same_named_claude_slot_health() {
    let t = fixture();
    seed_codex_snapshot(t.path(), "shared", "codex@example.com");
    seed_empty_claude_slot(t.path(), "shared");

    let (screen, code) = run_fullscreen_ui(t.path(), &[(300, b"q")]);
    assert_eq!(code, 0, "UI failed:\n{screen}");
    let start = screen
        .find("codex@example.com")
        .unwrap_or_else(|| panic!("Codex row missing:\n{screen}"));
    let row_tail: String = screen[start..].chars().take(240).collect();
    assert!(
        row_tail.contains("ready") && !row_tail.contains("no login"),
        "the Codex snapshot borrowed its same-named Claude slot's health:\n{row_tail}"
    );
}

#[test]
fn codex_slot_identity_outranks_same_provider_snapshot_copy() {
    let t = fixture();
    seed_codex_snapshot(t.path(), "shared", "old-copy@example.com");
    seed_codex_slot(t.path(), "shared", "live-slot@example.com");

    let (screen, code) = run_fullscreen_ui(t.path(), &[(300, b"q")]);
    assert_eq!(code, 0, "UI failed:\n{screen}");
    assert!(screen.contains("live-slot@example.com"), "{screen}");
    assert!(
        !screen.contains("old-copy@example.com"),
        "the live slot was labeled with its stale snapshot copy:\n{screen}"
    );
}

#[test]
fn healthy_codex_slot_does_not_inherit_broken_snapshot_warning() {
    let t = fixture();
    seed_codex_snapshot(t.path(), "shared", "old-copy@example.com");
    std::fs::write(
        t.path()
            .join(".local/share/swapdex/accounts/shared/codex/auth"),
        b"not json",
    )
    .unwrap();
    seed_codex_slot(t.path(), "shared", "live-slot@example.com");

    let (screen, code) = run_fullscreen_ui(t.path(), &[(300, b"q")]);
    assert_eq!(code, 0, "UI failed:\n{screen}");
    let start = screen
        .find("live-slot@example.com")
        .unwrap_or_else(|| panic!("Codex slot row missing:\n{screen}"));
    let row_tail: String = screen[start..].chars().take(240).collect();
    assert!(
        row_tail.contains("ready") && !row_tail.contains("unreadable"),
        "the healthy slot inherited its broken saved copy's warning:\n{row_tail}"
    );
}

#[test]
fn selecting_same_named_codex_row_switches_codex() {
    let t = fixture();
    seed_claude_snapshot(t.path(), "shared", "claude@example.com");
    let codex_dir = seed_codex_slot(t.path(), "shared", "codex@example.com");

    let (screen, code) = run_fullscreen_ui(t.path(), &[(300, b"2q")]);
    assert_eq!(code, 0, "UI failed:\n{screen}");
    let served = std::fs::read_to_string(t.path().join(".local/share/swapdex/serving-codex"))
        .unwrap_or_else(|e| {
            panic!("the selected Codex row did not set Codex serving: {e}\n{screen}")
        });
    assert_eq!(Path::new(served.trim()), codex_dir);
}

#[test]
fn deleting_same_named_codex_row_keeps_claude_snapshot() {
    let t = fixture();
    seed_claude_snapshot(t.path(), "shared", "claude@example.com");
    seed_codex_snapshot(t.path(), "shared", "codex@example.com");

    let (screen, code) = run_fullscreen_ui_after_screen(
        t.path(),
        &[(300, b"2d")],
        // The destructive-input guard requires 250 ms after the prompt opens.
        // Start that interval only after the real prompt has rendered: `2`
        // performs a blocking switch, so wall time since sending `2d` says
        // nothing about when `d` was processed.
        Some(("stop managing 'shared'?", 300, b"yq")),
    );
    assert_eq!(code, 0, "UI failed:\n{screen}");
    let account = t.path().join(".local/share/swapdex/accounts/shared");
    assert!(
        account.join("claude-code").is_dir(),
        "deleting the Codex row also deleted Claude"
    );
    assert!(
        !account.join("codex").exists(),
        "the selected Codex row was not deleted:\n{screen}"
    );
}
