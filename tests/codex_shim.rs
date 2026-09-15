//! Execute the installed shell launcher shape with isolated tools and homes.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use swapdex::shim::codex_shim_script;

struct Launch {
    args: Vec<String>,
    calls: String,
    stderr: String,
    home: String,
}

fn launch(args: &[&str], explicit_home: bool, repair_fails: bool) -> Launch {
    let root = tempfile::tempdir().unwrap();
    let tool = root.path().join("real codex");
    let sx = root.path().join("swapdex");
    let shim = root.path().join("shim");
    let pointer = root.path().join("active-codex");
    std::fs::write(&pointer, root.path().join("default home").to_str().unwrap()).unwrap();
    std::fs::write(&tool, "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME\" \"$@\"\n").unwrap();
    std::fs::write(&sx, r#"#!/bin/sh
printf '%s:%s\n' "$CODEX_HOME" "$*" >> "$SX_CALLS"
case "$1" in
  repair-codex-sessions) if [ "$SX_REPAIR_FAIL" = yes ]; then echo 'repair fixture failed' >&2; exit 1; fi ;;
  proxy) printf '%s\n' 8788 ;;
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
        .env("port", "9999")
        .env("sx_plain", "yes");
    if explicit_home {
        cmd.env("CODEX_HOME", root.path().join("chosen home"));
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let mut lines = stdout.lines();
    Launch {
        home: lines.next().unwrap().to_string(),
        args: lines.map(str::to_string).collect(),
        calls: std::fs::read_to_string(root.path().join("calls")).unwrap_or_default(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
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
            &got.args[..2],
            &["-c", "openai_base_url=http://127.0.0.1:8788/v1"],
            "{args:?}"
        );
        assert_eq!(&got.args[2..], args);
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
        &["--profile", "custom", "resume"],
        &["-pcustom", "resume"],
        &["--oss", "hello"],
        &["resume", "--remote", "unix:///tmp/server"],
        &["--remote-auth-token-env", "LOGIN_TOKEN", "resume"],
        &["-c", "openai_base_url=https://example.test/v1", "hello"],
    ] {
        let got = launch(args, false, false);
        assert_eq!(got.args, args, "{args:?}");
        assert!(!got.calls.contains("proxy"), "{args:?}: {}", got.calls);
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
        assert_eq!(&got.args[2..], args);
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
