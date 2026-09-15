//! Execute the installed Claude shell launcher with isolated tools and homes.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use swapdex::shim::shim_script;

struct LaunchAttempt {
    status: i32,
    native: String,
    calls: String,
    stderr: String,
}

fn launch_attempt(
    args: &[&str],
    proxy_stdout: &str,
    proxy_status: i32,
    extra_env: &[(&str, &str)],
) -> LaunchAttempt {
    let root = tempfile::tempdir().unwrap();
    let tool = root.path().join("real claude");
    let sx = root.path().join("swapdex");
    let shim = root.path().join("shim");
    let pointer = root.path().join("active-claude");
    std::fs::write(&pointer, root.path().join("default home").to_str().unwrap()).unwrap();
    std::fs::write(
        &tool,
        r#"#!/bin/sh
{
  printf 'base=%s\n' "${ANTHROPIC_BASE_URL-unset}"
  printf 'home=%s\n' "$CLAUDE_CONFIG_DIR"
  printf 'arg=%s\n' "$@"
} > "$NATIVE_CALL"
"#,
    )
    .unwrap();
    std::fs::write(
        &sx,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$SX_CALLS"
case "$1" in
  proxy) printf '%s' "$SX_PROXY_STDOUT"; exit "$SX_PROXY_STATUS" ;;
esac
"#,
    )
    .unwrap();
    std::fs::write(&shim, shim_script(&pointer, &tool, &sx)).unwrap();
    for path in [&tool, &sx, &shim] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let native_call = root.path().join("native-call");
    let mut command = Command::new("sh");
    command
        .arg(&shim)
        .args(args)
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_MANTLE")
        .env_remove("CLAUDE_CODE_USE_VERTEX")
        .env_remove("CLAUDE_CODE_USE_FOUNDRY")
        .env_remove("CLAUDE_CODE_USE_ANTHROPIC_AWS")
        .env("SX_CALLS", root.path().join("calls"))
        .env("SX_PROXY_STDOUT", proxy_stdout)
        .env("SX_PROXY_STATUS", proxy_status.to_string())
        .env("NATIVE_CALL", &native_call);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    LaunchAttempt {
        status: output.status.code().unwrap_or(-1),
        native: std::fs::read_to_string(native_call).unwrap_or_default(),
        calls: std::fs::read_to_string(root.path().join("calls")).unwrap_or_default(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

#[test]
fn managed_startup_failure_never_executes_native_claude() {
    for (status, stdout) in [(1, ""), (1, "8787"), (127, "8787")] {
        let got = launch_attempt(&[], stdout, status, &[]);
        assert_ne!(got.status, 0, "status={status}, stdout={stdout:?}");
        assert!(
            got.native.is_empty(),
            "native Claude ran after ensure status {status} with {stdout:?}"
        );
        assert!(
            got.stderr
                .contains("swapdex proxy --ensure --tool claude-code"),
            "{}",
            got.stderr
        );
    }
}

#[test]
fn invalid_success_output_never_executes_native_claude() {
    for stdout in [
        "",
        "port",
        "8787\n8788",
        "0",
        "-1",
        "65536",
        "99999999999999999999999999999999999999999999999999",
    ] {
        let got = launch_attempt(&[], stdout, 0, &[]);
        assert_ne!(got.status, 0, "stdout={stdout:?}");
        assert!(
            got.native.is_empty(),
            "native Claude ran with invalid port output {stdout:?}"
        );
    }
}

#[test]
fn valid_port_boundaries_route_claude_through_the_proxy() {
    for port in ["1", "8787", "65535"] {
        let got = launch_attempt(&["chat"], port, 0, &[]);
        assert_eq!(got.status, 0, "{}", got.stderr);
        assert!(
            got.native
                .contains(&format!("base=http://127.0.0.1:{port}")),
            "{}",
            got.native
        );
        assert!(got.native.contains("home=") && got.native.contains("default home"));
        assert!(got.native.contains("arg=chat"));
    }
}

#[test]
fn recognized_unmanaged_status_launches_claude_without_proxy_endpoint() {
    let got = launch_attempt(&["chat"], "", 3, &[]);
    assert_eq!(got.status, 0, "{}", got.stderr);
    assert!(got.native.contains("base=unset"), "{}", got.native);
    assert!(got.native.contains("arg=chat"));

    let malformed = launch_attempt(&["chat"], "8787", 3, &[]);
    assert_ne!(malformed.status, 0);
    assert!(malformed.native.is_empty());
}

#[test]
fn authentication_help_and_explicit_backends_bypass_managed_startup() {
    for args in [
        &["auth", "login"][..],
        &["auth", "logout"],
        &["auth", "status"],
        &["auth", "--help"],
        &["auth", "-h"],
        &["setup-token"],
        &["--verbose", "auth", "login"],
        &["--permission-mode", "plan", "auth", "status"],
        &["--model", "sonnet", "setup-token"],
        &["--plugin-dir", "plugin-a", "auth", "login"],
        &[
            "--plugin-url",
            "https://plugins.example.test/a.zip",
            "auth",
            "logout",
        ],
        &["--help"],
        &["--version"],
        &["-h"],
        &["-v"],
    ] {
        let got = launch_attempt(args, "", 1, &[]);
        assert_eq!(got.status, 0, "{args:?}: {}", got.stderr);
        assert!(got.calls.is_empty(), "{args:?}: {}", got.calls);
        assert!(!got.native.is_empty(), "{args:?}");
    }

    for (key, value) in [
        ("ANTHROPIC_BASE_URL", "https://gateway.example.test"),
        ("CLAUDE_CODE_USE_BEDROCK", "1"),
        ("CLAUDE_CODE_USE_MANTLE", "1"),
        ("CLAUDE_CODE_USE_VERTEX", "1"),
        ("CLAUDE_CODE_USE_FOUNDRY", "1"),
        ("CLAUDE_CODE_USE_ANTHROPIC_AWS", "1"),
    ] {
        let got = launch_attempt(&["chat"], "", 1, &[(key, value)]);
        assert_eq!(got.status, 0, "{key}: {}", got.stderr);
        assert!(got.calls.is_empty(), "{key}: {}", got.calls);
        assert!(!got.native.is_empty(), "{key}");
        if key == "ANTHROPIC_BASE_URL" {
            assert!(got.native.contains(value), "{}", got.native);
        }
    }

    for key in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    ] {
        for disabled in ["", "0", "false"] {
            let got = launch_attempt(&["chat"], "8787", 0, &[(key, disabled)]);
            assert_eq!(got.status, 0, "{key}={disabled:?}: {}", got.stderr);
            assert!(
                got.calls.contains("proxy --ensure"),
                "{key}={disabled:?}: {}",
                got.calls
            );
            assert!(got.native.contains("base=http://127.0.0.1:8787"));
        }
    }
}

#[test]
fn help_after_ambiguous_options_bypasses_managed_startup() {
    for args in [
        &["--debug", "--help"][..],
        &["-d", "--version"],
        &["--mcp-config", "one.json", "--help"],
        &["--debug", "auth", "--help"],
    ] {
        let got = launch_attempt(args, "", 1, &[]);
        assert_eq!(got.status, 0, "{args:?}: {}", got.stderr);
        assert!(got.calls.is_empty(), "{args:?}: {}", got.calls);
        assert!(!got.native.is_empty(), "{args:?}");
    }
}

#[test]
fn literal_and_option_value_help_stays_managed() {
    for args in [
        &["--", "--help"][..],
        &["--debug", "--", "--help"],
        &["--debug", "--model", "--help"],
        &["--mcp-config", "one.json", "--model", "--help"],
        &["--unknown-option", "--help"],
    ] {
        let got = launch_attempt(args, "8787", 0, &[]);
        assert_eq!(got.status, 0, "{args:?}: {}", got.stderr);
        assert!(
            got.calls.contains("proxy --ensure --tool claude-code"),
            "{args:?}: {}",
            got.calls
        );
        assert!(
            got.native.contains("base=http://127.0.0.1:8787"),
            "{args:?}: {}",
            got.native
        );
    }
}

#[test]
fn auth_words_in_prompts_and_option_values_stay_managed() {
    for args in [
        &["-p", "login"][..],
        &["--print", "login"],
        &["--", "login"],
        &["--", "auth", "login"],
        &["--model", "login", "say hi"],
        &["-m", "login", "say hi"],
        &["--permission-mode", "login", "say hi"],
        &["login"],
        &["/login"],
        &["logout"],
        &["/logout"],
    ] {
        let got = launch_attempt(args, "8787", 0, &[]);
        assert_eq!(got.status, 0, "{args:?}: {}", got.stderr);
        assert!(
            got.calls.contains("proxy --ensure --tool claude-code"),
            "{args:?}: {}",
            got.calls
        );
        assert!(
            got.native.contains("base=http://127.0.0.1:8787"),
            "{args:?}: {}",
            got.native
        );
    }
}

#[test]
fn optional_debug_value_never_selects_auth() {
    for args in [
        &["--debug", "auth", "login"][..],
        &["-d", "auth", "login"],
        &["--debug=auth", "login"],
    ] {
        let got = launch_attempt(args, "8787", 0, &[]);
        assert_eq!(got.status, 0, "{args:?}: {}", got.stderr);
        assert!(
            got.calls.contains("proxy --ensure --tool claude-code"),
            "{args:?}: {}",
            got.calls
        );
        assert!(
            got.native.contains("base=http://127.0.0.1:8787"),
            "{args:?}: {}",
            got.native
        );
    }
}

#[test]
fn variadic_mcp_config_values_never_select_auth() {
    for args in [
        &["--mcp-config", "one.json", "auth", "login"][..],
        &["--mcp-config", "one.json", "two.json", "auth", "login"],
        &["--mcp-config=one.json", "auth", "login"],
    ] {
        let got = launch_attempt(args, "8787", 0, &[]);
        assert_eq!(got.status, 0, "{args:?}: {}", got.stderr);
        assert!(
            got.calls.contains("proxy --ensure --tool claude-code"),
            "{args:?}: {}",
            got.calls
        );
        assert!(
            got.native.contains("base=http://127.0.0.1:8787"),
            "{args:?}: {}",
            got.native
        );
    }
}

fn swapdex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_swapdex"))
}

fn ensure(root: &Path, tool: &str) -> std::process::Output {
    Command::new(swapdex_bin())
        .args(["proxy", "--ensure", "--tool", tool])
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap()
}

fn store(root: &Path) -> PathBuf {
    root.join(".local/share/swapdex")
}

fn swapdex_build_id() -> String {
    let stamp = std::fs::metadata(swapdex_bin())
        .and_then(|metadata| metadata.modified())
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{}-{stamp}", env!("CARGO_PKG_VERSION"))
}

#[test]
fn actual_ensure_distinguishes_known_passthrough_from_state_errors() {
    for tool in ["claude-code", "codex"] {
        let no_accounts = tempfile::tempdir().unwrap();
        let output = ensure(no_accounts.path(), tool);
        assert_eq!(output.status.code(), Some(3), "{tool}: {output:?}");
        assert!(output.stdout.is_empty(), "{tool}: {output:?}");

        let empty_registry = tempfile::tempdir().unwrap();
        let data = store(empty_registry.path());
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("slots.json"), b"[]").unwrap();
        let output = ensure(empty_registry.path(), tool);
        assert_eq!(output.status.code(), Some(3), "{tool}: {output:?}");
        assert!(output.stdout.is_empty(), "{tool}: {output:?}");

        let explicit_off = tempfile::tempdir().unwrap();
        let data = store(explicit_off.path());
        std::fs::create_dir_all(&data).unwrap();
        let short = if tool == "claude-code" {
            "claude"
        } else {
            tool
        };
        std::fs::write(data.join(format!("serving-{short}")), b"off").unwrap();
        std::fs::write(data.join("slots.json"), b"corrupt registry").unwrap();
        let output = ensure(explicit_off.path(), tool);
        assert_eq!(output.status.code(), Some(3), "{tool}: {output:?}");
        assert!(output.stdout.is_empty(), "{tool}: {output:?}");

        let marker = if tool == "claude-code" {
            "proxy"
        } else {
            "proxy-codex"
        };
        std::fs::write(
            data.join(marker),
            format!("{} 45678 {}\n", std::process::id(), swapdex_build_id()),
        )
        .unwrap();
        let output = ensure(explicit_off.path(), tool);
        assert_eq!(output.status.code(), Some(0), "{tool}: {output:?}");
        assert_eq!(output.stdout, b"45678\n", "{tool}: {output:?}");

        for pointer in [format!("serving-{short}"), format!("active-{short}")] {
            for invalid in [b"relative".as_slice(), &[0xff][..]] {
                let invalid_state = tempfile::tempdir().unwrap();
                let data = store(invalid_state.path());
                std::fs::create_dir_all(&data).unwrap();
                std::fs::write(data.join("slots.json"), b"[]").unwrap();
                std::fs::write(data.join(&pointer), invalid).unwrap();
                let output = ensure(invalid_state.path(), tool);
                assert_eq!(output.status.code(), Some(1), "{tool}: {output:?}");
                assert!(!output.stderr.is_empty(), "{tool}: {output:?}");
            }

            let dangling = tempfile::tempdir().unwrap();
            let data = store(dangling.path());
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("slots.json"), b"[]").unwrap();
            std::fs::write(data.join(&pointer), b"/missing/account-slot").unwrap();
            let output = ensure(dangling.path(), tool);
            assert_eq!(output.status.code(), Some(1), "{tool}: {output:?}");
            assert!(!output.stderr.is_empty(), "{tool}: {output:?}");

            let dangling_link = tempfile::tempdir().unwrap();
            let data = store(dangling_link.path());
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("slots.json"), b"[]").unwrap();
            std::os::unix::fs::symlink("missing-pointer-target", data.join(&pointer)).unwrap();
            let output = ensure(dangling_link.path(), tool);
            assert_eq!(output.status.code(), Some(1), "{tool}: {output:?}");
            assert!(!output.stderr.is_empty(), "{tool}: {output:?}");
        }

        for pointer in [format!("serving-{short}"), format!("active-{short}")] {
            let unreadable_state = tempfile::tempdir().unwrap();
            let data = store(unreadable_state.path());
            std::fs::create_dir_all(data.join(pointer)).unwrap();
            std::fs::write(data.join("slots.json"), b"[]").unwrap();
            let output = ensure(unreadable_state.path(), tool);
            assert_eq!(output.status.code(), Some(1), "{tool}: {output:?}");
            assert!(!output.stderr.is_empty(), "{tool}: {output:?}");
        }

        let corrupt_registry = tempfile::tempdir().unwrap();
        let data = store(corrupt_registry.path());
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("slots.json"), b"corrupt registry").unwrap();
        let output = ensure(corrupt_registry.path(), tool);
        assert_eq!(output.status.code(), Some(1), "{tool}: {output:?}");
        assert!(!output.stderr.is_empty(), "{tool}: {output:?}");

        let dangling_registry = tempfile::tempdir().unwrap();
        let data = store(dangling_registry.path());
        std::fs::create_dir_all(&data).unwrap();
        std::os::unix::fs::symlink("missing-registry-target", data.join("slots.json")).unwrap();
        let output = ensure(dangling_registry.path(), tool);
        assert_eq!(output.status.code(), Some(1), "{tool}: {output:?}");
        assert!(!output.stderr.is_empty(), "{tool}: {output:?}");
    }
}
