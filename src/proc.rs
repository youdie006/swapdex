//! Best-effort detection of a running `claude` / `codex` process, used to warn
//! before a switch that could disrupt a live session (a running session holds
//! the old token and can overwrite the just-switched login on its next refresh).
//!
//! Local only - never the network. Matching is on the exact process name so a
//! stray `~/.claude/...` path in some unrelated command line never trips it: we
//! would rather miss a node-wrapped session (safe) than raise a false alarm.

/// Command names of currently-running processes (best-effort; empty on failure).
pub fn running_process_names() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        linux_comms()
    }
    #[cfg(not(target_os = "linux"))]
    {
        ps_comms()
    }
}

#[cfg(target_os = "linux")]
fn linux_comms() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let is_pid = e
                .file_name()
                .to_str()
                .map(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .unwrap_or(false);
            if is_pid {
                if let Ok(comm) = std::fs::read_to_string(e.path().join("comm")) {
                    out.push(comm.trim().to_string());
                }
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn ps_comms() -> Vec<String> {
    match std::process::Command::new("ps")
        .args(["-Ao", "comm="])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().rsplit('/').next().unwrap_or("").to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A running Claude Code process and the login "slot" it uses. The slot is the
/// `CLAUDE_CONFIG_DIR` (or `CLAUDE_SECURESTORAGE_CONFIG_DIR`) it was launched
/// with - the same thing that decides its Keychain item. `env_read == false`
/// means its environment could not be inspected, so its slot is UNKNOWN and the
/// guard must fail closed (it might be the very slot being swapped).
#[derive(Debug, Clone)]
pub struct ClaudeProc {
    pub securestorage_dir: Option<String>,
    pub config_dir: Option<String>,
    pub env_read: bool,
}

/// The login slot key for a Claude process/environment, mirroring how the
/// Keychain service is derived: `CLAUDE_SECURESTORAGE_CONFIG_DIR` wins when set
/// (empty string means the bare/default slot), else `CLAUDE_CONFIG_DIR`, else
/// the bare slot. `None` == the bare slot. Not path-canonicalized: Claude keys
/// on the raw string, so two spellings of the same dir are two slots (matching
/// Claude's own behavior).
fn slot_key(securestorage: Option<&str>, config: Option<&str>) -> Option<String> {
    match securestorage {
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(s.trim().to_string()),
        None => match config {
            Some(c) if !c.trim().is_empty() => Some(c.trim().to_string()),
            _ => None,
        },
    }
}

/// The verdict of the pre-switch guard for one tool.
#[derive(Debug, PartialEq, Eq)]
pub enum GuardVerdict {
    /// No running session uses the slot being swapped - safe to switch.
    Clear,
    /// A running session uses this exact slot; switching will clobber the new
    /// login and revoke the outgoing account's snapshot on its next refresh.
    SameSlot,
    /// A running Claude session's slot could not be determined - fail closed
    /// (it might be this slot).
    Unknown,
}

/// Decide whether swapping the Claude slot `my` (swapdex's own
/// securestorage/config dir, `None` = bare) is safe, given the running Claude
/// processes. A CONFIRMED same-slot session wins over any Unknown; Unknown wins
/// over Clear (fail closed).
pub fn claude_switch_guard(
    my_securestorage: Option<&str>,
    my_config: Option<&str>,
    running: &[ClaudeProc],
) -> GuardVerdict {
    let mine = slot_key(my_securestorage, my_config);
    let mut unknown = false;
    for p in running {
        if !p.env_read {
            unknown = true;
            continue;
        }
        if slot_key(p.securestorage_dir.as_deref(), p.config_dir.as_deref()) == mine {
            return GuardVerdict::SameSlot;
        }
    }
    if unknown {
        GuardVerdict::Unknown
    } else {
        GuardVerdict::Clear
    }
}

/// Every running Claude Code process with the login slot it uses (best-effort;
/// empty on failure). Feeds `claude_switch_guard`.
pub fn running_claude_procs() -> Vec<ClaudeProc> {
    #[cfg(target_os = "linux")]
    {
        linux_claude_procs()
    }
    #[cfg(not(target_os = "linux"))]
    {
        macos_claude_procs()
    }
}

/// Pull the two CLAUDE config-dir vars out of a NUL-separated `/proc/<pid>/environ`
/// blob (Linux). Pure, so it is unit-tested directly.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn config_dirs_from_environ(bytes: &[u8]) -> (Option<String>, Option<String>) {
    let (mut ss, mut cfg) = (None, None);
    for kv in bytes.split(|&b| b == 0) {
        let Ok(s) = std::str::from_utf8(kv) else {
            continue;
        };
        if let Some(v) = s.strip_prefix("CLAUDE_SECURESTORAGE_CONFIG_DIR=") {
            ss = Some(v.to_string());
        } else if let Some(v) = s.strip_prefix("CLAUDE_CONFIG_DIR=") {
            cfg = Some(v.to_string());
        }
    }
    (ss, cfg)
}

#[cfg(target_os = "linux")]
fn linux_claude_procs() -> Vec<ClaudeProc> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let is_pid = e
            .file_name()
            .to_str()
            .map(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false);
        if !is_pid {
            continue;
        }
        let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        if comm.trim() != "claude" {
            continue;
        }
        // environ is readable only for our own processes; a failure (EACCES for
        // another user's claude, or a race) means "slot unknown" -> fail closed.
        match std::fs::read(e.path().join("environ")) {
            Ok(bytes) => {
                let (securestorage_dir, config_dir) = config_dirs_from_environ(&bytes);
                out.push(ClaudeProc {
                    securestorage_dir,
                    config_dir,
                    env_read: true,
                });
            }
            Err(_) => out.push(ClaudeProc {
                securestorage_dir: None,
                config_dir: None,
                env_read: false,
            }),
        }
    }
    out
}

/// Pull the CLAUDE config-dir vars out of a `ps eww` line, which appends the
/// environment after the command as space-separated `KEY=VALUE`. `env_seen`
/// reports whether the environment was actually included (a sentinel like
/// `PATH=`/`HOME=` is present): on macOS `ps e` shows the env only for our own
/// processes, so its ABSENCE must be treated as "unknown", not "bare slot".
/// Pure, so it is unit-tested directly.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn config_dirs_from_ps(line: &str) -> (Option<String>, Option<String>, bool) {
    let mut ss = None;
    let mut cfg = None;
    let mut env_seen = false;
    for tok in line.split_ascii_whitespace() {
        if tok.starts_with("PATH=") || tok.starts_with("HOME=") || tok.starts_with("USER=") {
            env_seen = true;
        }
        if let Some(v) = tok.strip_prefix("CLAUDE_SECURESTORAGE_CONFIG_DIR=") {
            ss = Some(v.to_string());
            env_seen = true;
        } else if let Some(v) = tok.strip_prefix("CLAUDE_CONFIG_DIR=") {
            cfg = Some(v.to_string());
            env_seen = true;
        }
    }
    (ss, cfg, env_seen)
}

#[cfg(not(target_os = "linux"))]
fn macos_claude_procs() -> Vec<ClaudeProc> {
    // 1) claude pids (exact binary name, mirroring tool_running).
    let listing = match std::process::Command::new("ps")
        .args(["-Ao", "pid=,comm="])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return Vec::new(),
    };
    let pids: Vec<String> = listing
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let (pid, comm) = l.split_once(char::is_whitespace)?;
            let name = comm.trim().rsplit('/').next().unwrap_or("");
            (name == "claude").then(|| pid.trim().to_string())
        })
        .collect();
    // 2) per pid: `ps eww` appends the environment for our own processes.
    let mut out = Vec::new();
    for pid in pids {
        match std::process::Command::new("ps")
            .args(["eww", "-o", "command=", "-p", &pid])
            .output()
        {
            Ok(o) if o.status.success() => {
                let line = String::from_utf8_lossy(&o.stdout);
                let (securestorage_dir, config_dir, env_seen) = config_dirs_from_ps(&line);
                out.push(ClaudeProc {
                    securestorage_dir,
                    config_dir,
                    env_read: env_seen,
                });
            }
            // Could not read env for a claude we know is running -> unknown slot.
            _ => out.push(ClaudeProc {
                securestorage_dir: None,
                config_dir: None,
                env_read: false,
            }),
        }
    }
    out
}

/// Does a process for `tool` (an adapter id) appear to be running? Best-effort:
/// exact-match the tool's binary name against the collected process names.
pub fn tool_running(tool: &str, comms: &[String]) -> bool {
    let want = match tool {
        "claude-code" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "antigravity" => "agy",
        _ => return false,
    };
    comms.iter().any(|c| c == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_binary_name_only() {
        let comms = vec![
            "codex".to_string(),
            "bash".to_string(),
            "some-claude-helper".to_string(), // must NOT match claude
        ];
        assert!(tool_running("codex", &comms));
        assert!(!tool_running("claude-code", &comms), "no exact 'claude'");
        assert!(!tool_running("unknown", &comms));
    }

    #[test]
    fn matches_claude_when_present() {
        let comms = vec!["claude".to_string(), "node".to_string()];
        assert!(tool_running("claude-code", &comms));
        assert!(!tool_running("codex", &comms));
    }

    fn proc(config: Option<&str>) -> ClaudeProc {
        ClaudeProc {
            securestorage_dir: None,
            config_dir: config.map(str::to_string),
            env_read: true,
        }
    }

    #[test]
    fn slot_key_derivation_matches_keychain_rules() {
        // bare slot: nothing set, or empty strings.
        assert_eq!(slot_key(None, None), None);
        assert_eq!(slot_key(None, Some("")), None);
        assert_eq!(slot_key(Some(""), Some("/x")), None); // securestorage empty = bare, wins
                                                          // config dir sets the slot.
        assert_eq!(
            slot_key(None, Some("/home/u/.claude-company")).as_deref(),
            Some("/home/u/.claude-company")
        );
        // securestorage wins over config.
        assert_eq!(slot_key(Some("/ss"), Some("/cfg")).as_deref(), Some("/ss"));
    }

    #[test]
    fn guard_flags_only_a_session_on_the_same_slot() {
        // swapdex swaps the BARE slot; a company-slot session is irrelevant.
        let company = proc(Some("/home/u/.claude-company"));
        assert_eq!(
            claude_switch_guard(None, None, std::slice::from_ref(&company)),
            GuardVerdict::Clear
        );
        // A plain `claude` (bare slot) running while we swap the bare slot: danger.
        let bare = proc(None);
        assert_eq!(
            claude_switch_guard(None, None, &[company.clone(), bare]),
            GuardVerdict::SameSlot
        );
        // Swapping the company slot while only a bare session runs: safe.
        assert_eq!(
            claude_switch_guard(None, Some("/home/u/.claude-company"), &[proc(None)]),
            GuardVerdict::Clear
        );
        // Swapping the company slot while a company session runs: danger.
        assert_eq!(
            claude_switch_guard(None, Some("/home/u/.claude-company"), &[company]),
            GuardVerdict::SameSlot
        );
    }

    #[test]
    fn environ_blob_yields_the_config_dirs() {
        // NUL-separated KEY=VALUE, as in /proc/<pid>/environ.
        let blob = b"PATH=/usr/bin\0CLAUDE_CONFIG_DIR=/home/u/.claude-company\0TERM=xterm\0";
        let (ss, cfg) = config_dirs_from_environ(blob);
        assert_eq!(ss, None);
        assert_eq!(cfg.as_deref(), Some("/home/u/.claude-company"));
        // A bare-slot claude: no CONFIG_DIR set.
        let (_, cfg2) = config_dirs_from_environ(b"PATH=/usr/bin\0HOME=/home/u\0");
        assert_eq!(cfg2, None);
        // securestorage present.
        let (ss3, _) = config_dirs_from_environ(
            b"CLAUDE_SECURESTORAGE_CONFIG_DIR=/ss\0CLAUDE_CONFIG_DIR=/c\0",
        );
        assert_eq!(ss3.as_deref(), Some("/ss"));
    }

    #[test]
    fn ps_line_parses_env_and_flags_visibility() {
        // `ps eww` line: command then env tokens. PATH= present -> env visible.
        let line = "/opt/homebrew/bin/node claude PATH=/usr/bin CLAUDE_CONFIG_DIR=/home/u/.claude-company TERM=xterm";
        let (ss, cfg, seen) = config_dirs_from_ps(line);
        assert_eq!(ss, None);
        assert_eq!(cfg.as_deref(), Some("/home/u/.claude-company"));
        assert!(seen, "PATH= present means the environment was included");
        // No env tokens at all (env NOT visible) -> must not be read as bare slot.
        let (_, cfg2, seen2) = config_dirs_from_ps("/opt/homebrew/bin/node claude");
        assert_eq!(cfg2, None);
        assert!(!seen2, "no env visible -> caller must fail closed");
    }

    // End-to-end on Linux (also WSL): spawn a process whose `comm` is exactly
    // "claude" with CLAUDE_CONFIG_DIR set, and confirm running_claude_procs()
    // enumerates it and reads its slot. Proves the /proc/<pid>/{comm,environ}
    // path the guard relies on. (macOS `ps eww` path is verified on a Mac.)
    #[cfg(target_os = "linux")]
    #[test]
    fn running_claude_procs_reads_a_live_sessions_config_dir() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("swapdex_proc_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A binary literally named `claude` so /proc/<pid>/comm == "claude".
        let bin = dir.join("claude");
        std::fs::copy("/bin/sleep", &bin)
            .or_else(|_| std::fs::copy("/usr/bin/sleep", &bin))
            .unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", "/home/tester/.claude-company")
            .spawn()
            .unwrap();
        // Retry the whole detection, not just a /proc existence poll: on a loaded
        // CI runner the child can take a moment to be published in /proc with a
        // readable environ, and a single shot after the poll can still race.
        let mut mine = None;
        let mut last = Vec::new();
        for _ in 0..150 {
            last = running_claude_procs();
            if let Some(p) = last
                .iter()
                .find(|p| p.config_dir.as_deref() == Some("/home/tester/.claude-company"))
            {
                mine = Some(p.clone());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);

        let mine = mine.unwrap_or_else(|| {
            panic!(
                "the spawned claude session's CLAUDE_CONFIG_DIR should be detected; got {last:?}"
            )
        });
        assert!(mine.env_read, "env was readable for our own process");
        let _ = std::io::stdout().flush();
    }

    #[test]
    fn guard_fails_closed_on_unreadable_env() {
        let unknown = ClaudeProc {
            securestorage_dir: None,
            config_dir: None,
            env_read: false,
        };
        // An unreadable session could be on this slot - do not clear it.
        assert_eq!(
            claude_switch_guard(None, None, std::slice::from_ref(&unknown)),
            GuardVerdict::Unknown
        );
        // No running claude at all: nothing to guard against.
        assert_eq!(claude_switch_guard(None, None, &[]), GuardVerdict::Clear);
        // A confirmed same-slot session outweighs an unknown one.
        assert_eq!(
            claude_switch_guard(None, None, &[unknown, proc(None)]),
            GuardVerdict::SameSlot
        );
    }
}
