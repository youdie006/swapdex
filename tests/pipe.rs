//! A Unix tool piped into a short reader ends quietly.
//!
//! Rust sets SIGPIPE to SIG_IGN before main, so a closed pipe turns the next
//! `println!` into a panic. `swapdex ls | head -1` printed a Rust panic and a
//! backtrace hint where `ls`, `git` and every other tool simply stop.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

/// Run `<swapdex ARGS> | head -1` and hand back whatever went to stderr.
fn piped_into_head(args: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("'{}' {args} | head -1", bin()))
        .env(
            "SWAPDEX_ROOT",
            std::env::temp_dir().join("swapdex-pipe-test"),
        )
        .output()
        .expect("run the pipeline");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_closed_pipe_is_not_a_panic() {
    for args in ["ls", "service status", "doctor"] {
        let err = piped_into_head(args);
        assert!(
            !err.contains("panicked"),
            "`swapdex {args} | head -1` panicked instead of ending quietly:\n{err}"
        );
        assert!(
            !err.contains("Broken pipe"),
            "`swapdex {args} | head -1` complained about the pipe:\n{err}"
        );
    }
}
