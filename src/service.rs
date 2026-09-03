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
    let port = crate::commands::default_port_for(tool);
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
    <string>--port</string>
    <string>{port}</string>
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
    let port = crate::commands::default_port_for(tool);
    format!(
        "[Unit]\n\
         Description=swapdex {tool} proxy\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe} proxy --tool {tool} --port {port}\n\
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
/// The program a service unit will run, read back from the unit itself.
///
/// The proxy is the one part whose failure takes every session down at once, so
/// the health check has to be able to see what the unit actually points at. The
/// path is resolved at install time; for an npm install it contains the Node
/// version, so upgrading Node deletes it and the proxy silently never starts
/// again - the service still reads as installed.
pub fn unit_program(body: &str) -> Option<&str> {
    if let Some(line) = body
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))
    {
        return line.split_whitespace().next();
    }
    // launchd: the first <string> after <key>ProgramArguments</key>.
    let rest = body.split("ProgramArguments").nth(1)?;
    let open = rest.find("<string>")? + "<string>".len();
    let close = rest[open..].find("</string>")? + open;
    Some(rest[open..close].trim())
}

/// `systemctl show -p NRestarts --value` prints the count, or nothing at all
/// for a unit it does not know. Empty is not zero: a supervisor that did not
/// answer has not told us the proxy is fine.
pub fn parse_nrestarts(out: &str) -> Option<u32> {
    out.trim().parse().ok()
}

/// `launchctl list <label>` prints a plist-ish block holding LastExitStatus,
/// or an error line when the job is not loaded. launchd keeps no restart
/// count, so this is the whole of what it can say.
pub fn parse_launchctl_last_exit(out: &str) -> Option<i32> {
    out.lines()
        .find_map(|l| l.split_once("\"LastExitStatus\" = "))
        .and_then(|(_, rest)| rest.trim().trim_end_matches(';').parse().ok())
}

/// Ask whichever supervisor is on this machine. Both answers stay None when
/// the supervisor could not be reached, so silence is never read as health.
pub fn supervisor_report(tool: &str) -> (Option<u32>, Option<i32>) {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("launchctl")
            .args(["list", &launchd_label(tool)])
            .output()
            .ok();
        let text = out
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        (None, parse_launchctl_last_exit(&text))
    } else {
        let out = std::process::Command::new("systemctl")
            .args([
                "--user",
                "show",
                &systemd_unit(tool),
                "-p",
                "NRestarts",
                "--value",
            ])
            .output()
            .ok();
        let text = out
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        (parse_nrestarts(&text), None)
    }
}

/// What the supervisor knows about a proxy that is not staying up.
///
/// `service status` used to answer one question - is something listening - so a
/// proxy that had crashed and been restarted five times that day printed the
/// same line as one that had been up for a week. The supervisor was counting
/// the whole time; nothing asked it.
///
/// The two supervisors know different things, so each says only its own:
/// systemd keeps a restart count, launchd keeps only the last exit status.
pub fn supervision_note(restarts: Option<u32>, last_exit: Option<i32>) -> Option<String> {
    if let Some(n) = restarts {
        if n > 0 {
            let how = if n == 1 {
                "once".to_string()
            } else {
                format!("{n} times")
            };
            return Some(format!("restarted {how} - it is not staying up"));
        }
    }
    match last_exit {
        Some(0) | None => None,
        Some(c) => Some(format!("last exit {c}")),
    }
}

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
    // `quiet` marks a step whose failure is the NORMAL case - booting out an
    // agent that was never loaded. Printing launchctl's "Input/output error"
    // there made a clean first install read as if something had gone wrong.
    let run = |prog: &str, args: &[&str], quiet: bool| {
        let out = std::process::Command::new(prog).args(args).output();
        match out {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr);
                let e = e.trim();
                if !e.is_empty() && !quiet {
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
        // label, and "already there" is the normal case on a re-install. Nothing
        // to boot out is equally normal, which is why that step says nothing.
        let target = format!("gui/{}", unsafe { libc::getuid() });
        run("launchctl", &["bootout", &target, &p], true);
        run("launchctl", &["bootstrap", &target, &p], false);
    } else {
        run("systemctl", &["--user", "daemon-reload"], false);
        run(
            "systemctl",
            &["--user", "enable", "--now", &systemd_unit(tool)],
            false,
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

    /// Each agent must name its OWN port. Without one the unit takes the CLI
    /// default, which is Claude's - so the Codex agent tried to bind a port the
    /// Claude agent already held, exited 1, and was restarted into that failure
    /// by the supervisor. `proxy --ensure` knew to add one for Codex; the unit
    /// did not, because it does not go through `--ensure`.
    #[test]
    fn each_agent_carries_the_port_it_should_bind() {
        let claude = launchd_plist(Path::new("/x/swapdex"), "claude-code", Path::new("/l"));
        let codex = launchd_plist(Path::new("/x/swapdex"), "codex", Path::new("/l"));
        assert!(claude.contains("<string>8787</string>"), "{claude}");
        assert!(codex.contains("<string>8788</string>"), "{codex}");
        let u = systemd_service(Path::new("/x/swapdex"), "codex");
        assert!(u.contains("--port 8788"), "{u}");
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

#[cfg(test)]
mod unit_program_tests {
    use super::*;

    /// The health check must be able to see what a service unit will run.
    ///
    /// The proxy service is the one part whose failure takes every session down
    /// at once, and `doctor` did not look at it at all. The unit records an
    /// absolute path resolved at install time; for an npm install that path
    /// contains the Node version, so upgrading Node deletes it and the proxy
    /// silently never starts again. That is exactly the shape of the outage
    /// that took an hour to name: the service reads as installed, and nothing
    /// says the binary it points at is gone.
    #[test]
    fn the_program_a_unit_will_run_can_be_read_back() {
        let systemd = "[Unit]\nDescription=x\n[Service]\n\
                       ExecStart=/opt/swapdex/bin/swapdex proxy --tool claude-code --port 8787\n\
                       Restart=always\n";
        assert_eq!(unit_program(systemd), Some("/opt/swapdex/bin/swapdex"));

        let plist = "<plist><dict>\n<key>ProgramArguments</key>\n<array>\n\
                     <string>/usr/local/bin/swapdex</string>\n<string>proxy</string>\n\
                     </array>\n</dict></plist>\n";
        assert_eq!(unit_program(plist), Some("/usr/local/bin/swapdex"));

        assert_eq!(unit_program("nothing here"), None);
    }
}

#[cfg(test)]
mod supervision_note_tests {
    use super::*;

    #[test]
    fn a_proxy_that_stays_up_gets_no_note() {
        assert_eq!(supervision_note(Some(0), Some(0)), None);
        assert_eq!(supervision_note(None, None), None);
    }

    /// The whole point. `swapdex service status` said "proxy: running" for a
    /// proxy that had died and been restarted five times that day, because it
    /// asked whether one was listening and nothing else. systemd counts the
    /// restarts; swapdex simply never asked.
    #[test]
    fn systemd_restarts_are_reported() {
        let note = supervision_note(Some(5), Some(1)).expect("five restarts is worth saying");
        assert!(note.contains('5'), "should name the count: {note}");
    }

    #[test]
    fn one_restart_reads_as_english() {
        let note = supervision_note(Some(1), None).expect("one restart is still worth saying");
        assert!(note.contains("once"), "not 'restarted 1 times': {note}");
    }

    /// launchd keeps no restart count, only the last exit status, so on macOS
    /// that is all there is to say - and saying it is better than silence.
    #[test]
    fn launchd_reports_the_last_exit_instead() {
        assert_eq!(supervision_note(None, Some(0)), None);
        let note = supervision_note(None, Some(137)).expect("a non-zero exit is worth saying");
        assert!(note.contains("137"), "should name the status: {note}");
    }
}

#[cfg(test)]
mod supervisor_parse_tests {
    use super::*;

    #[test]
    fn systemd_restart_count() {
        assert_eq!(parse_nrestarts("5\n"), Some(5));
        assert_eq!(parse_nrestarts("0\n"), Some(0));
        // A unit systemd does not know answers empty, and that is not zero.
        assert_eq!(parse_nrestarts("\n"), None);
        assert_eq!(parse_nrestarts("[not-set]"), None);
    }

    #[test]
    fn launchctl_last_exit_status() {
        let block = "{\n\t\"Label\" = \"io.github.youdie006.swapdex.claude\";\n\
                     \t\"LastExitStatus\" = 137;\n\t\"PID\" = 4242;\n}";
        assert_eq!(parse_launchctl_last_exit(block), Some(137));
        let ok = "{\n\t\"LastExitStatus\" = 0;\n}";
        assert_eq!(parse_launchctl_last_exit(ok), Some(0));
        // Not loaded: launchctl prints an error, not a block.
        assert_eq!(
            parse_launchctl_last_exit("Could not find service in domain"),
            None
        );
    }
}
