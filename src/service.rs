//! Run the proxy as a managed service, so it outlives a terminal and comes back
//! by itself.
//!
//! The proxy is started by the shim on demand today, which has two costs. It dies
//! with whatever shell started it, and - the one that actually bit - it inherits
//! that shell's ability to read credentials. Started from an ssh session on macOS,
//! it cannot open the Keychain, so it answers every turn by forwarding the
//! client's own login. A launchd AGENT runs in the user's own login session and
//! has that access, which is the whole point of using one.

use crate::paths::Paths;
use std::path::{Path, PathBuf};

/// The reverse-DNS label launchd knows the agent by, one per tool.
pub fn launchd_label(tool: &str) -> String {
    format!("io.github.youdie006.swapdex.{}", short_tool(tool))
}

/// The systemd unit name, one per tool.
pub fn systemd_unit(tool: &str) -> String {
    format!("swapdex-{}.service", short_tool(tool))
}

fn short_tool(tool: &str) -> &'static str {
    match tool {
        "codex" => "codex",
        _ => "claude",
    }
}

/// A launchd agent that keeps one tool's proxy up.
///
/// `KeepAlive` restarts it if it stops, which is what makes killing the proxy a
/// recoverable mistake rather than a day of confusion. Output goes to a file
/// rather than nowhere: a proxy that logs to /dev/null is one whose reasons
/// cannot be read afterwards, and that is how a broken one went unnoticed.
pub fn launchd_plist(exe: &Path, tool: &str, log_dir: &Path) -> String {
    let label = launchd_label(tool);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>proxy</string>
    <string>--tool</string>
    <string>{tool}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}/{label}.log</string>
  <key>StandardErrorPath</key><string>{log}/{label}.log</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        log = log_dir.display(),
    )
}

/// A systemd USER unit - not a system one. The proxy holds one person's
/// credentials and must run as that person, never as root.
pub fn systemd_service(exe: &Path, tool: &str) -> String {
    format!(
        "[Unit]\n\
         Description=swapdex {tool} proxy\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe} proxy --tool {tool}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
    )
}

/// Where the agent file belongs on macOS.
pub fn launchd_path(home: &Path, tool: &str) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{}.plist", launchd_label(tool)))
}

/// Where the unit belongs on Linux.
pub fn systemd_path(home: &Path, tool: &str) -> PathBuf {
    home.join(".config/systemd/user").join(systemd_unit(tool))
}

/// Where a managed proxy writes what it says.
pub fn log_dir(paths: &Paths) -> PathBuf {
    paths.store_dir().join("logs")
}

/// Install (or replace) the agent for one tool and start it.
///
/// The binary path is written in FULL, resolved now: an agent that looked a name
/// up on PATH would be at the mercy of whatever a login shell happens to set, and
/// that is precisely the ambiguity that made two installs of swapdex fight.
pub fn install(paths: &Paths, tool: &str) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("cannot find swapdex's own path")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let home = dirs::home_dir().context("cannot determine home dir")?;
    let logs = log_dir(paths);
    std::fs::create_dir_all(&logs).ok();

    let (path, body) = if cfg!(target_os = "macos") {
        (launchd_path(&home, tool), launchd_plist(&exe, tool, &logs))
    } else {
        (systemd_path(&home, tool), systemd_service(&exe, tool))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create the service directory")?;
    }
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    // Stop whatever the shim already started, or the agent's own proxy cannot
    // bind the port - and with KeepAlive set, the supervisor would restart it into
    // that same failure for as long as the machine is on.
    stop_running(paths, tool);
    load(&path, tool);
    Ok(path)
}

/// Stop a proxy the shim started, so the supervised one can take the port.
fn stop_running(paths: &Paths, tool: &str) {
    let Some((pid, _, _)) = crate::proxy::running_proxy_for(paths, tool) else {
        return;
    };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..40 {
        if crate::proxy::running_proxy_for(paths, tool).is_none() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Hand the unit to the supervisor. Failures are reported, never fatal: the file
/// is written either way, and `launchctl`/`systemctl` can be run by hand.
fn load(path: &Path, tool: &str) {
    let run = |prog: &str, args: &[&str]| {
        let out = std::process::Command::new(prog).args(args).output();
        match out {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr);
                let e = e.trim();
                if !e.is_empty() {
                    eprintln!("swapdex: {prog} said: {e}");
                }
                false
            }
            Err(e) => {
                eprintln!("swapdex: could not run {prog}: {e}");
                false
            }
        }
    };
    if cfg!(target_os = "macos") {
        let p = path.display().to_string();
        // Replace rather than add: bootstrap fails outright on an already-loaded
        // label, and "already there" is the normal case on a re-install.
        let target = format!("gui/{}", unsafe { libc::getuid() });
        run("launchctl", &["bootout", &target, &p]);
        run("launchctl", &["bootstrap", &target, &p]);
    } else {
        run("systemctl", &["--user", "daemon-reload"]);
        run(
            "systemctl",
            &["--user", "enable", "--now", &systemd_unit(tool)],
        );
    }
}

/// Remove the agent and stop it.
pub fn uninstall(tool: &str) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context;
    let home = dirs::home_dir().context("cannot determine home dir")?;
    let path = if cfg!(target_os = "macos") {
        launchd_path(&home, tool)
    } else {
        systemd_path(&home, tool)
    };
    if !path.exists() {
        return Ok(None);
    }
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}", unsafe { libc::getuid() });
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &target, &path.display().to_string()])
            .output();
    } else {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", &systemd_unit(tool)])
            .output();
    }
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_restarts_the_proxy_and_keeps_its_output() {
        let p = launchd_plist(
            Path::new("/opt/homebrew/bin/swapdex"),
            "claude-code",
            Path::new("/tmp/logs"),
        );
        assert!(
            p.contains("<key>KeepAlive</key><true/>"),
            "it comes back: {p}"
        );
        assert!(
            p.contains("<key>RunAtLoad</key><true/>"),
            "and starts at login"
        );
        assert!(
            p.contains("/opt/homebrew/bin/swapdex"),
            "the real binary, not a name off PATH"
        );
        assert!(
            p.contains("--tool") && p.contains("claude-code"),
            "one agent per tool: {p}"
        );
        assert!(
            p.contains("/tmp/logs/io.github.youdie006.swapdex.claude.log"),
            "its output is kept, because a proxy that logs nowhere cannot be diagnosed: {p}"
        );
    }

    #[test]
    fn the_two_tools_get_separate_agents() {
        assert_ne!(launchd_label("claude-code"), launchd_label("codex"));
        assert_ne!(systemd_unit("claude-code"), systemd_unit("codex"));
    }

    /// A user unit, never a system one: this process holds one person's
    /// credentials, and running it as root would put them somewhere anyone with
    /// the machine could reach.
    #[test]
    fn systemd_runs_as_the_user_and_restarts() {
        let u = systemd_service(Path::new("/usr/local/bin/swapdex"), "codex");
        assert!(u.contains("Restart=always"), "{u}");
        assert!(u.contains("WantedBy=default.target"), "a user unit: {u}");
        assert!(!u.contains("multi-user.target"), "never a system unit: {u}");
        assert!(!u.contains("User="), "it runs as whoever enabled it: {u}");
    }

    #[test]
    fn the_files_land_where_the_supervisor_looks() {
        let home = Path::new("/Users/x");
        assert_eq!(
            launchd_path(home, "claude-code"),
            home.join("Library/LaunchAgents/io.github.youdie006.swapdex.claude.plist")
        );
        assert_eq!(
            systemd_path(home, "codex"),
            home.join(".config/systemd/user/swapdex-codex.service")
        );
    }
}
