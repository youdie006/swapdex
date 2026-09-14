//! An npm update can unlink the ELF while its picker is still open on Linux.
#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Picker {
    child: Child,
    terminal: File,
    output: String,
}

impl Picker {
    fn start(binary: &Path, root: &Path, curl: &Path) -> Self {
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize {
            ws_row: 35,
            ws_col: 170,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let terminal = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut command = Command::new(binary);
        command
            .arg("ui")
            .env("SWAPDEX_ROOT", root)
            .env("SWAPDEX_CURL", curl)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let fd = terminal.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            assert_ne!(flags, -1);
            assert_ne!(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK), -1);
        }
        Self {
            child,
            terminal,
            output: String::new(),
        }
    }

    fn wait_until(&mut self, predicate: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let mut buf = [0u8; 65536];
            match self.terminal.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    if text.contains("\x1b[6n") {
                        self.terminal.write_all(b"\x1b[1;1R").unwrap();
                    }
                    self.output.push_str(&text);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Ok(_) => {}
                Err(e) => panic!("picker terminal: {e}\n{}", self.output),
            }
            if predicate(&self.output) {
                return;
            }
            assert!(self.child.try_wait().unwrap().is_none(), "picker exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("picker did not reach expected state:\n{}", self.output);
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        // The child owns this session, including its short-lived quota helpers.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

fn exercise_picker(unlink: bool) {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots/rnd");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"FAKE","refreshToken":"FAKE","expiresAt":9999999999000}}"#,
    ).unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        br#"{"oauthAccount":{"accountUuid":"synthetic-rnd","emailAddress":"rnd@example.com"}}"#,
    )
    .unwrap();
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec(&serde_json::json!([{
            "name":"rnd", "id":"rnd", "tool":"claude-code",
            "config_dir":slot, "adopted":false
        }]))
        .unwrap(),
    )
    .unwrap();
    let curl = root.path().join("fake-curl");
    std::fs::write(&curl, "#!/bin/sh\ncat >/dev/null\nprintf '{}\\n503'\n").unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let binary = root.path().join("swapdex");
    std::fs::copy(env!("CARGO_BIN_EXE_swapdex"), &binary).unwrap();
    let mut picker = Picker::start(&binary, root.path(), &curl);
    picker.wait_until(|s| s.contains("rnd@example.com"));
    if unlink {
        std::fs::remove_file(&binary).unwrap();
    }
    picker.terminal.write_all(b"\r").unwrap();
    if unlink {
        picker.wait_until(|s| s.contains("reopen swapdex") || s.contains("os error 2"));
        assert!(
            picker.output.contains("reopen swapdex"),
            "an updated picker must explain how to recover:\n{}",
            picker.output
        );
        assert!(!store.join("serving-claude").exists());
    } else {
        picker.wait_until(|_| store.join("serving-claude").is_file());
        assert_eq!(
            std::fs::read_to_string(store.join("serving-claude"))
                .unwrap()
                .trim(),
            slot.to_str().unwrap()
        );
    }
}

#[test]
fn updated_picker_explains_restart_without_changing_the_selected_account() {
    exercise_picker(true);
}

#[test]
fn freshly_opened_picker_can_select_the_account() {
    exercise_picker(false);
}
