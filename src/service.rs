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

/// The short name in a unit file or launchd label, one per tool.
///
/// `claude` and `codex` are fixed: units under those names are already
/// installed on real machines and renaming them would orphan them. The other
/// two used to fall through to "claude", so `service install --tool gemini`
/// overwrote the Claude service with one that ran the Gemini proxy.
fn short_tool(tool: &str) -> &'static str {
    match tool {
        "codex" => "codex",
        "gemini" => "gemini",
        "antigravity" => "antigravity",
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

/// Where this machine's supervisor keeps one tool's unit, derived from the
/// paths in hand rather than the ambient home.
///
/// `install` used to ask `dirs::home_dir()`, which `SWAPDEX_ROOT` cannot
/// redirect - a sandboxed run would have written a real launchd agent or
/// systemd unit into the user's home.
pub fn unit_path(paths: &Paths, tool: &str) -> PathBuf {
    if cfg!(target_os = "macos") {
        launchd_path(paths.home(), tool)
    } else {
        systemd_path(paths.home(), tool)
    }
}

/// Whether these paths describe the real machine, rather than a test root.
///
/// Writing the unit somewhere harmless is not containment on its own: asking
/// launchctl or systemctl to load it would still reach the real supervisor.
pub fn manages_the_real_machine(paths: &Paths) -> bool {
    dirs::home_dir().is_some_and(|h| h == paths.home())
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

/// The command that asks this machine's supervisor to restart one tool's proxy.
///
/// Pure, and takes the platform rather than reading it, so both shapes are
/// tested on either machine.
pub fn restart_argv(tool: &str, macos: bool, uid: u32) -> Vec<String> {
    if macos {
        vec![
            "launchctl".into(),
            "kickstart".into(),
            // Without -k an already-running job is left exactly as it is.
            "-k".into(),
            format!("gui/{uid}/{}", launchd_label(tool)),
        ]
    } else {
        vec![
            "systemctl".into(),
            "--user".into(),
            "restart".into(),
            systemd_unit(tool),
        ]
    }
}

/// Ask the supervisor to restart the proxy. False when there is no supervisor
/// for this tool, or it would not do it - the caller then falls back to the
/// signal, because not replacing a stale proxy at all is worse.
pub fn restart_via_supervisor(home: Option<&Path>, tool: &str) -> bool {
    let macos = cfg!(target_os = "macos");
    let installed = home.is_some_and(|h| {
        if macos {
            launchd_path(h, tool).exists()
        } else {
            systemd_path(h, tool).exists()
        }
    });
    if !installed {
        return false;
    }
    let argv = restart_argv(tool, macos, unsafe { libc::getuid() });
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .is_ok_and(|o| o.status.success())
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
pub fn supervisor_report(tool: &str) -> (Option<u32>, Option<i32>, Option<u64>) {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("launchctl")
            .args(["list", &launchd_label(tool)])
            .output()
            .ok();
        let text = out
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        (
            None,
            parse_launchctl_last_exit(&text),
            launchd_uptime_secs(&text),
        )
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
        (parse_nrestarts(&text), None, systemd_uptime_secs(tool))
    }
}

/// `ps -o etime=` as seconds. The format is `[[DD-]HH:]MM:SS`, right-padded.
///
/// `etime` and not `etimes`: the seconds form is a Linux procps extension and
/// does not exist on macOS, which is the only platform that reaches the
/// launchd path at all.
pub fn parse_etime(out: &str) -> Option<u64> {
    let t = out.trim();
    if t.is_empty() {
        return None;
    }
    let (days, rest) = match t.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, t),
    };
    let mut parts = rest.split(':').rev();
    let secs: u64 = parts.next()?.parse().ok()?;
    let mins: u64 = parts.next()?.parse().ok()?;
    let hours: u64 = match parts.next() {
        Some(h) => h.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(days * 86400 + hours * 3600 + mins * 60 + secs)
}

/// How long the running job has been up, via the PID launchctl reports. None
/// when it is not running or `ps` will not say - which keeps a note that
/// cannot be dated in its present-tense form rather than guessing.
pub fn launchd_uptime_secs(list_output: &str) -> Option<u64> {
    let pid: i32 = list_output
        .lines()
        .find_map(|l| l.split_once("\"PID\" = "))
        .and_then(|(_, r)| r.trim().trim_end_matches(';').parse().ok())?;
    let out = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    parse_etime(&String::from_utf8_lossy(&out.stdout))
}

/// The same for systemd, from the unit's own activation timestamp.
pub fn systemd_uptime_secs(tool: &str) -> Option<u64> {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            &systemd_unit(tool),
            "-p",
            "ActiveEnterTimestampMonotonic",
            "--value",
        ])
        .output()
        .ok()?;
    let entered: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if entered == 0 {
        return None;
    }
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return None;
    }
    let now_us = ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000;
    now_us.checked_sub(entered).map(|d| d / 1_000_000)
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
///
/// Both are HISTORY, and history is not a claim about now. A codex proxy on a
/// real machine had been up for twenty-five hours with launchd still holding a
/// `9` from the run before; saying "last exit 9" flat made a healthy proxy
/// read as a broken one. So the uptime decides the wording: still short means
/// the alarm is live, long enough means it is something that happened once.
pub fn supervision_note(
    restarts: Option<u32>,
    last_exit: Option<i32>,
    uptime_secs: Option<u64>,
) -> Option<String> {
    // An hour: long enough that a proxy replaced on an upgrade, or restarted
    // once overnight, is plainly running now; short enough that a crash loop
    // never reaches it.
    let settled = uptime_secs.is_some_and(|u| u >= 3600);
    let since = uptime_secs.map(for_how_long).unwrap_or_default();

    if let Some(n) = restarts {
        if n > 0 {
            let how = if n == 1 {
                "once".to_string()
            } else {
                format!("{n} times")
            };
            return Some(if settled {
                format!("restarted {how}, but up {since} since")
            } else {
                format!("restarted {how} - it is not staying up")
            });
        }
    }
    match last_exit {
        Some(0) | None => None,
        Some(c) if settled => Some(format!("a previous run exited {c}; up {since} since")),
        Some(c) => Some(format!("last exit {c}")),
    }
}

/// A duration a person reads at a glance, not a precise one.
fn for_how_long(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

pub fn install(paths: &Paths, tool: &str) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("cannot find swapdex's own path")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let logs = log_dir(paths);
    std::fs::create_dir_all(&logs).ok();

    let path = unit_path(paths, tool);
    let (path, body) = if cfg!(target_os = "macos") {
        (path, launchd_plist(&exe, tool, &logs))
    } else {
        (path, systemd_service(&exe, tool))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create the service directory")?;
    }
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    // Stop whatever the shim already started, or the agent's own proxy cannot
    // bind the port - and with KeepAlive set, the supervisor would restart it into
    // that same failure for as long as the machine is on.
    stop_running(paths, tool);
    // Only the real machine's supervisor gets driven. Under a test root the
    // unit now lands inside the sandbox, but handing that path to launchctl or
    // systemctl would still load it for real.
    if manages_the_real_machine(paths) {
        load(&path, tool);
    }
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
pub fn uninstall(paths: &Paths, tool: &str) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context;
    let path = unit_path(paths, tool);
    if !path.exists() {
        return Ok(None);
    }
    if manages_the_real_machine(paths) {
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
        assert_eq!(supervision_note(Some(0), Some(0), None), None);
        assert_eq!(supervision_note(None, None, None), None);
    }

    /// A count is not a claim about now. On a real machine a codex proxy had
    /// been up for twenty-five hours with launchd still holding a `9` from the
    /// run before it - and the first version of this said "last exit 9" as
    /// though something were wrong at that moment. Same for a restart count
    /// that a long uptime has since outlived.
    #[test]
    fn history_is_not_reported_as_a_present_problem() {
        let day = 25 * 3600;
        let old_exit = supervision_note(None, Some(9), Some(day)).expect("still worth mentioning");
        assert!(
            old_exit.contains("previous") || old_exit.contains("since"),
            "must place it in the past: {old_exit}"
        );
        assert!(!old_exit.contains("not staying up"), "not now: {old_exit}");

        let old_restarts = supervision_note(Some(5), None, Some(day)).expect("worth mentioning");
        assert!(
            !old_restarts.contains("not staying up"),
            "it plainly is staying up: {old_restarts}"
        );
        assert!(
            old_restarts.contains('5'),
            "the count still shows: {old_restarts}"
        );
    }

    /// The alarm still fires when the uptime is short, which is what a proxy
    /// dying every few minutes actually looks like.
    #[test]
    fn a_short_uptime_with_restarts_is_still_an_alarm() {
        let note = supervision_note(Some(5), None, Some(90)).expect("this one is real");
        assert!(note.contains("not staying up"), "should alarm: {note}");
    }

    /// The whole point. `swapdex service status` said "proxy: running" for a
    /// proxy that had died and been restarted five times that day, because it
    /// asked whether one was listening and nothing else. systemd counts the
    /// restarts; swapdex simply never asked.
    #[test]
    fn systemd_restarts_are_reported() {
        let note = supervision_note(Some(5), Some(1), None).expect("five restarts is worth saying");
        assert!(note.contains('5'), "should name the count: {note}");
    }

    #[test]
    fn one_restart_reads_as_english() {
        let note =
            supervision_note(Some(1), None, None).expect("one restart is still worth saying");
        assert!(note.contains("once"), "not 'restarted 1 times': {note}");
    }

    /// launchd keeps no restart count, only the last exit status, so on macOS
    /// that is all there is to say - and saying it is better than silence.
    #[test]
    fn launchd_reports_the_last_exit_instead() {
        assert_eq!(supervision_note(None, Some(0), None), None);
        let note =
            supervision_note(None, Some(137), None).expect("a non-zero exit is worth saying");
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

#[cfg(test)]
mod restart_argv_tests {
    use super::*;

    /// Replacing a proxy the supervisor owns by signalling it makes the
    /// supervisor count a crash: every routine upgrade of swapdex added one to
    /// NRestarts, so the "it is not staying up" note would cry wolf on a
    /// machine where nothing was wrong. Ask the supervisor instead.
    #[test]
    fn systemd_is_asked_to_restart_its_own_unit() {
        let argv = restart_argv("claude-code", false, 1000);
        assert_eq!(
            argv,
            vec![
                "systemctl".to_string(),
                "--user".into(),
                "restart".into(),
                "swapdex-claude.service".into()
            ]
        );
    }

    /// launchd needs the -k so the running job is stopped first; a plain
    /// kickstart on an already-running job does nothing at all.
    #[test]
    fn launchd_kickstarts_the_agent_in_the_user_domain() {
        let argv = restart_argv("codex", true, 501);
        assert_eq!(
            argv,
            vec![
                "launchctl".to_string(),
                "kickstart".into(),
                "-k".into(),
                "gui/501/io.github.youdie006.swapdex.codex".into()
            ]
        );
    }
}

#[cfg(test)]
mod supervisor_sandbox_tests {
    use super::*;

    /// It must consult the home it is GIVEN, not the ambient one. The first
    /// wiring called `dirs::home_dir()`, which a `SWAPDEX_ROOT` sandbox cannot
    /// redirect, and the test suite restarted the developer's own proxy twice
    /// before anyone noticed.
    ///
    /// What this can check on a machine with no service installed is only that
    /// an empty home yields false; on a machine that has one, it also proves
    /// the ambient home was not consulted.
    #[test]
    fn an_empty_home_has_no_supervisor_to_ask() {
        let home = tempfile::tempdir().unwrap();
        assert!(!restart_via_supervisor(Some(home.path()), "claude-code"));
        assert!(!restart_via_supervisor(Some(home.path()), "codex"));
    }

    #[test]
    fn no_home_at_all_is_not_a_supervisor_either() {
        assert!(!restart_via_supervisor(None, "claude-code"));
    }
}

#[cfg(test)]
mod etime_tests {
    use super::*;

    /// macOS `ps` has no `etimes` - that is a Linux procps extension, and the
    /// first version asked for it on the one platform this code path serves,
    /// so it would have answered None forever and the stale note would have
    /// stayed. `etime` exists on both; these are its real shapes, copied from
    /// the machine that showed the bug.
    #[test]
    fn etime_is_parsed_in_every_shape_ps_prints() {
        assert_eq!(parse_etime("01-01:06:48"), Some(90408)); // 1d 1h 6m 48s
        assert_eq!(parse_etime("01-05:27:51"), Some(106071)); // 86400+18000+1620+51
        assert_eq!(parse_etime("   01:02:03"), Some(3723)); // padded HH:MM:SS
        assert_eq!(parse_etime("      12:34"), Some(754)); // padded MM:SS
        assert_eq!(parse_etime("00:00"), Some(0));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("not a duration"), None);
    }
}

#[cfg(test)]
mod sandbox_containment_tests {
    use super::*;

    /// `SWAPDEX_ROOT` promises that a run cannot touch the real machine, and
    /// `install` was computing the unit's location from `dirs::home_dir()` -
    /// so a sandboxed install would have written a real launchd agent or
    /// systemd unit into the user's home and asked the supervisor to load it.
    #[test]
    fn the_unit_lands_under_the_paths_home_not_the_ambient_one() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(root.path());
        for tool in ["claude-code", "codex"] {
            let p = unit_path(&paths, tool);
            assert!(
                p.starts_with(root.path()),
                "{tool} unit escaped the sandbox: {}",
                p.display()
            );
        }
    }

    /// And a sandboxed run must not drive the real supervisor either: writing
    /// the file somewhere harmless is no good if launchctl or systemctl is
    /// then asked to load it.
    #[test]
    fn a_sandboxed_run_does_not_drive_the_supervisor() {
        let root = tempfile::tempdir().unwrap();
        assert!(!manages_the_real_machine(&crate::paths::Paths::rooted(
            root.path()
        )));
    }
}

#[cfg(test)]
mod tool_identity_tests {
    use super::*;

    /// Four tools are offered on `--tool`, and every one that was not codex
    /// collapsed onto "claude". `service install --tool gemini` therefore wrote
    /// a file called `swapdex-claude.service` whose ExecStart ran the GEMINI
    /// proxy - replacing the user's Claude service, on the port Claude is
    /// pinned to.
    #[test]
    fn every_tool_gets_its_own_unit_and_label() {
        let tools = ["claude-code", "codex", "gemini", "antigravity"];
        let units: Vec<String> = tools.iter().map(|t| systemd_unit(t)).collect();
        let labels: Vec<String> = tools.iter().map(|t| launchd_label(t)).collect();
        for i in 0..tools.len() {
            for j in (i + 1)..tools.len() {
                assert_ne!(
                    units[i], units[j],
                    "{} and {} share a unit file",
                    tools[i], tools[j]
                );
                assert_ne!(
                    labels[i], labels[j],
                    "{} and {} share a launchd label",
                    tools[i], tools[j]
                );
            }
        }
    }

    /// The names already installed on real machines must not move.
    #[test]
    fn the_two_that_already_exist_keep_their_names() {
        assert_eq!(systemd_unit("claude-code"), "swapdex-claude.service");
        assert_eq!(systemd_unit("codex"), "swapdex-codex.service");
    }

    /// Two proxies cannot share a port, and the unit is written with one.
    #[test]
    fn every_tool_gets_its_own_port() {
        let tools = ["claude-code", "codex", "gemini", "antigravity"];
        let ports: Vec<u16> = tools
            .iter()
            .map(|t| crate::commands::default_port_for(t))
            .collect();
        for i in 0..tools.len() {
            for j in (i + 1)..tools.len() {
                assert_ne!(
                    ports[i], ports[j],
                    "{} and {} both want port {}",
                    tools[i], tools[j], ports[i]
                );
            }
        }
    }
}
