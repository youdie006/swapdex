//! Execute the installed shell launcher shape with isolated tools and homes.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use swapdex::shim::codex_shim_script;

static FIXTURE_EXEC_LOCK: Mutex<()> = Mutex::new(());

fn fixture_exec_lock() -> MutexGuard<'static, ()> {
    FIXTURE_EXEC_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct LaunchAttempt {
    status: i32,
    stdout: String,
    calls: String,
    stderr: String,
    native_invoked: bool,
}

struct Launch {
    args: Vec<String>,
    calls: String,
    stderr: String,
    home: String,
}

fn launch_attempt(
    args: &[&str],
    explicit_home: bool,
    repair_fails: bool,
    proxy_stdout: &str,
    proxy_status: i32,
) -> LaunchAttempt {
    // Tests in this binary run concurrently. Serialize executable fixture writes
    // and forks so a child cannot retain another fixture's writable descriptor
    // long enough for execve to reject that fixture with ETXTBSY.
    let _exec_guard = fixture_exec_lock();
    let root = tempfile::tempdir().unwrap();
    let tool = root.path().join("real codex");
    let sx = root.path().join("swapdex");
    let shim = root.path().join("shim");
    let pointer = root.path().join("active-codex");
    std::fs::write(&pointer, root.path().join("default home").to_str().unwrap()).unwrap();
    std::fs::write(
        &tool,
        "#!/bin/sh\nprintf '%s\\n' invoked > \"$NATIVE_CALL\"\nprintf '%s\\n' \"$CODEX_HOME\" \"$@\"\n",
    )
    .unwrap();
    std::fs::write(&sx, r#"#!/bin/sh
printf '%s:%s\n' "$CODEX_HOME" "$*" >> "$SX_CALLS"
case "$1" in
  repair-codex-sessions) if [ "$SX_REPAIR_FAIL" = yes ]; then echo 'repair fixture failed' >&2; exit 1; fi ;;
  proxy) printf '%s' "$SX_PROXY_STDOUT"; exit "$SX_PROXY_STATUS" ;;
  serve) printf '%s\n' work ;;
esac
"#).unwrap();
    std::fs::write(&shim, codex_shim_script(&pointer, &tool, &sx)).unwrap();
    for p in [&tool, &sx, &shim] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cmd = Command::new("sh");
    cmd.arg(&shim)
        .args(args)
        .env_remove("CODEX_HOME")
        .env("SX_CALLS", root.path().join("calls"))
        .env("SX_REPAIR_FAIL", if repair_fails { "yes" } else { "no" })
        .env("SX_PROXY_STDOUT", proxy_stdout)
        .env("SX_PROXY_STATUS", proxy_status.to_string())
        .env("NATIVE_CALL", root.path().join("native-call"))
        .env("port", "9999")
        .env("sx_plain", "yes");
    if explicit_home {
        cmd.env("CODEX_HOME", root.path().join("chosen home"));
    }
    let out = cmd.output().unwrap();
    LaunchAttempt {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8(out.stdout).unwrap(),
        calls: std::fs::read_to_string(root.path().join("calls")).unwrap_or_default(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        native_invoked: root.path().join("native-call").exists(),
    }
}

fn launch(args: &[&str], explicit_home: bool, repair_fails: bool) -> Launch {
    let attempt = launch_attempt(args, explicit_home, repair_fails, "8788", 0);
    assert_eq!(attempt.status, 0, "{}", attempt.stderr);
    assert!(attempt.native_invoked, "native Codex was not launched");
    let mut lines = attempt.stdout.lines();
    Launch {
        home: lines.next().unwrap().to_string(),
        args: lines.map(str::to_string).collect(),
        calls: attempt.calls,
        stderr: attempt.stderr,
    }
}

#[test]
fn managed_startup_failure_never_executes_native_codex() {
    for (status, stdout) in [(1, ""), (1, "8788"), (127, "8788")] {
        let got = launch_attempt(&["resume"], false, false, stdout, status);
        assert_ne!(got.status, 0, "status={status}, stdout={stdout:?}");
        assert!(
            !got.native_invoked,
            "native Codex ran after ensure status {status} with {stdout:?}"
        );
        assert!(got.stderr.contains("swapdex proxy --ensure --tool codex"));
    }
}

#[test]
fn invalid_success_output_never_executes_native_codex() {
    for stdout in [
        "",
        "port",
        "8788\n8789",
        "0",
        "-1",
        "65536",
        "99999999999999999999999999999999999999999999999999",
    ] {
        let got = launch_attempt(&["resume"], false, false, stdout, 0);
        assert_ne!(got.status, 0, "stdout={stdout:?}");
        assert!(
            !got.native_invoked,
            "native Codex ran with invalid port output {stdout:?}"
        );
        assert!(got.stderr.contains("swapdex proxy --ensure --tool codex"));
    }
}

#[test]
fn valid_port_boundaries_route_codex_through_the_stable_provider() {
    for port in ["1", "8788", "65535"] {
        let got = launch_attempt(&["resume"], false, false, port, 0);
        assert_eq!(got.status, 0, "{}", got.stderr);
        assert!(got.native_invoked, "port {port}");
        assert!(
            got.stdout
                .lines()
                .any(|line| line == format!("openai_base_url=http://127.0.0.1:{port}/v1")),
            "{}",
            got.stdout
        );
        assert!(!got.stdout.contains("model_provider"));
    }
}

#[test]
fn recognized_unmanaged_status_launches_codex_without_proxy_config() {
    let got = launch_attempt(&["resume"], false, false, "", 3);
    assert_eq!(got.status, 0, "{}", got.stderr);
    assert!(got.native_invoked);
    assert_eq!(got.stdout.lines().last(), Some("resume"));
    assert!(!got.stdout.contains("openai_base_url"));

    let malformed = launch_attempt(&["resume"], false, false, "8788", 3);
    assert_ne!(malformed.status, 0);
    assert!(!malformed.native_invoked);
}

#[test]
fn native_launch_resume_and_exec_share_one_stable_provider() {
    for args in [
        &[][..],
        &["resume"][..],
        &["resume", "--all"],
        &["resume", "--last"],
        &["resume", "00000000-0000-0000-0000-000000000001"],
        &["fork", "--last"],
        &["exec", "hello"],
    ] {
        let got = launch(args, false, false);
        assert_eq!(
            &got.args[..4],
            &[
                "-c",
                "openai_base_url=http://127.0.0.1:8788/v1",
                "-c",
                "chatgpt_base_url=http://127.0.0.1:8788/backend-api/"
            ],
            "{args:?}"
        );
        assert_eq!(&got.args[4..], args);
        assert!(!got.args.iter().any(|a| a.contains("model_provider")));
        assert!(got.calls.contains("repair-codex-sessions --quiet"));
        assert!(
            got.calls.find("repair-codex-sessions").unwrap()
                < got.calls.find("proxy --ensure").unwrap()
        );
        assert!(got.home.ends_with("default home"));
        assert!(got.calls.lines().all(|l| l.starts_with(&got.home)));
    }
}

#[test]
fn auth_help_and_explicit_backend_choices_bypass_routing() {
    for args in [
        &["login"][..],
        &["logout"],
        &["--image=one.png", "login"],
        &["--help"],
        &["--version"],
        &["-C", "/tmp/project", "login"],
        &["-c", "model=example", "login"],
        &["-c", "model_provider=other", "resume"],
        &["--config=model_provider=other", "resume"],
        &["-c", " model_provider = \"other\" ", "resume"],
        &[
            "--config= openai_base_url = \"https://example.test/v1\"",
            "resume",
        ],
        &["--oss", "hello"],
        &["resume", "--remote", "unix:///tmp/server"],
        &["--remote-auth-token-env", "LOGIN_TOKEN", "resume"],
        &["-c", "openai_base_url=https://example.test/v1", "hello"],
    ] {
        let got = launch_attempt(args, false, false, "", 1);
        assert_eq!(got.status, 0, "{args:?}: {}", got.stderr);
        assert!(got.native_invoked, "{args:?}");
        let got_args: Vec<_> = got.stdout.lines().skip(1).collect();
        assert_eq!(got_args, args, "{args:?}");
        assert!(!got.calls.contains("proxy"), "{args:?}: {}", got.calls);
    }
}

#[test]
fn profile_choices_remain_managed_and_reach_codex_unchanged() {
    for args in [
        &["-p", "worker", "resume"][..],
        &["--profile", "worker", "resume"],
        &["-pworker", "resume"],
        &["--profile=worker", "resume"],
    ] {
        let got = launch(args, true, false);
        assert_eq!(
            &got.args[..4],
            &[
                "-c",
                "openai_base_url=http://127.0.0.1:8788/v1",
                "-c",
                "chatgpt_base_url=http://127.0.0.1:8788/backend-api/"
            ],
            "{args:?}"
        );
        assert_eq!(&got.args[4..], args, "{args:?}");
        assert!(got.home.ends_with("chosen home"));
    }
}

#[test]
fn prompt_words_and_option_values_do_not_select_a_command() {
    for args in [
        &["exec", "login"][..],
        &["exec", "resume"],
        &["exec", "--", "logout"],
        &["--", "login"],
        &["-C", "login", "resume"],
        &["-m", "login", "resume"],
        &["-c", "model=login", "resume"],
        &["--image", "one.png", "login"],
    ] {
        let got = launch(args, true, false);
        assert!(
            got.args
                .iter()
                .any(|a| a == "openai_base_url=http://127.0.0.1:8788/v1"),
            "{args:?}"
        );
        assert_eq!(&got.args[4..], args);
        assert!(got.home.ends_with("chosen home"));
    }
}

#[test]
fn a_repair_problem_is_visible_and_does_not_discard_the_launch() {
    let got = launch(&["resume"], false, true);
    assert!(got.stderr.contains("repair fixture failed"));
    assert!(got.stderr.contains("repair-codex-sessions"));
    assert_eq!(got.args.last().unwrap(), "resume");
}
