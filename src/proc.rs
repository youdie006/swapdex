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
    /// The `HOME` it was launched with - what a bare session's config dir
    /// resolves from. `None` means it could not be read.
    pub home: Option<String>,
    pub env_read: bool,
}

/// The Keychain key for a Claude process/environment, mirroring how the service
/// name is derived: `CLAUDE_SECURESTORAGE_CONFIG_DIR` wins when set (empty
/// string means the bare/default service), else `CLAUDE_CONFIG_DIR`, else the
/// bare service. `None` == the bare service. Deliberately NOT path-normalized:
/// the service name is a hash of the raw string, so two spellings really are
/// two Keychain items. The FILES under the config dir are a separate axis -
/// see `same_login_slot`.
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

/// The login-slot inputs of one environment: swapdex's own, or a running
/// process's.
#[derive(Debug, Clone, Copy)]
struct SlotEnv<'a> {
    securestorage: Option<&'a str>,
    config: Option<&'a str>,
    home: Option<&'a str>,
}

/// The config directory an environment actually reads: `CLAUDE_CONFIG_DIR` when
/// set and non-empty, else `<HOME>/.claude`. `None` when neither is known.
fn config_dir_of(e: &SlotEnv) -> Option<std::path::PathBuf> {
    fn nonempty(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|x| !x.is_empty())
    }
    if let Some(c) = nonempty(e.config) {
        return Some(std::path::PathBuf::from(c));
    }
    nonempty(e.home).map(|h| std::path::Path::new(h).join(".claude"))
}

/// Are these two environments on the same login slot - would a switch on one
/// overwrite what the other holds?
///
/// A login lives in two places, so there are two axes. The config dir holds
/// files - `.credentials.json` on Linux, `.claude.json` (the identity) on every
/// platform - so equal DIRECTORIES are one login however the env spells them,
/// and a bare session's directory is `<HOME>/.claude`. The macOS Keychain item
/// is the other: its service name is a hash of the raw env string, and the BARE
/// service is shared across HOMEs because the login keychain belongs to the OS
/// user, not to `HOME` - that is what `keychain_shared` stands for.
///
/// When a directory cannot be resolved on either side, fall back to comparing
/// the raw keys: knowing less must not make the guard quieter.
fn same_login_slot(a: &SlotEnv, b: &SlotEnv, keychain_shared: bool) -> bool {
    let raw_equal = slot_key(a.securestorage, a.config) == slot_key(b.securestorage, b.config);
    if keychain_shared && raw_equal {
        return true;
    }
    match (config_dir_of(a), config_dir_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => raw_equal,
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
    my_home: Option<&str>,
    running: &[ClaudeProc],
) -> GuardVerdict {
    let mine = SlotEnv {
        securestorage: my_securestorage,
        config: my_config,
        home: my_home,
    };
    let shared = cfg!(target_os = "macos");
    let mut unknown = false;
    for p in running {
        if !p.env_read {
            unknown = true;
            continue;
        }
        let theirs = SlotEnv {
            securestorage: p.securestorage_dir.as_deref(),
            config: p.config_dir.as_deref(),
            home: p.home.as_deref(),
        };
        if same_login_slot(&mine, &theirs, shared) {
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

/// Read one process's login slot out of a NUL-separated `/proc/<pid>/environ`
/// blob (Linux). `HOME` counts: a session with no `CLAUDE_CONFIG_DIR` reads
/// `<HOME>/.claude`. Reaching this at all means the environ was readable, so
/// `env_read` is true. Pure, so it is unit-tested directly.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn config_dirs_from_environ(bytes: &[u8]) -> ClaudeProc {
    let mut p = ClaudeProc {
        securestorage_dir: None,
        config_dir: None,
        home: None,
        env_read: true,
    };
    for kv in bytes.split(|&b| b == 0) {
        let Ok(s) = std::str::from_utf8(kv) else {
            continue;
        };
        if let Some(v) = s.strip_prefix("CLAUDE_SECURESTORAGE_CONFIG_DIR=") {
            p.securestorage_dir = Some(v.to_string());
        } else if let Some(v) = s.strip_prefix("CLAUDE_CONFIG_DIR=") {
            p.config_dir = Some(v.to_string());
        } else if let Some(v) = s.strip_prefix("HOME=") {
            p.home = Some(v.to_string());
        }
    }
    p
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
            Ok(bytes) => out.push(config_dirs_from_environ(&bytes)),
            Err(_) => out.push(ClaudeProc {
                securestorage_dir: None,
                config_dir: None,
                home: None,
                env_read: false,
            }),
        }
    }
    out
}

/// Read one process's login slot out of a `ps eww` line, which appends the
/// environment after the command as space-separated `KEY=VALUE`. `env_read`
/// reports whether the environment was actually included (a sentinel like
/// `PATH=`/`HOME=` is present): on macOS `ps e` shows the env only for our own
/// processes, so its ABSENCE must be treated as "unknown", not "bare slot".
/// `HOME` is both that sentinel and a value we need - a session with no
/// `CLAUDE_CONFIG_DIR` reads `<HOME>/.claude`. Pure, so it is unit-tested
/// directly.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn config_dirs_from_ps(line: &str) -> ClaudeProc {
    let mut p = ClaudeProc {
        securestorage_dir: None,
        config_dir: None,
        home: None,
        env_read: false,
    };
    for tok in line.split_ascii_whitespace() {
        if tok.starts_with("PATH=") || tok.starts_with("USER=") {
            p.env_read = true;
        }
        if let Some(v) = tok.strip_prefix("CLAUDE_SECURESTORAGE_CONFIG_DIR=") {
            p.securestorage_dir = Some(v.to_string());
            p.env_read = true;
        } else if let Some(v) = tok.strip_prefix("CLAUDE_CONFIG_DIR=") {
            p.config_dir = Some(v.to_string());
            p.env_read = true;
        } else if let Some(v) = tok.strip_prefix("HOME=") {
            p.home = Some(v.to_string());
            p.env_read = true;
        }
    }
    p
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
                out.push(config_dirs_from_ps(&line));
            }
            // Could not read env for a claude we know is running -> unknown slot.
            _ => out.push(ClaudeProc {
                securestorage_dir: None,
                config_dir: None,
                home: None,
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

/// The environment a tool reads its login slot from: the variable that names
/// the slot outright, and the directory under `HOME` a session that sets
/// nothing falls back to.
struct SlotVars {
    comm: &'static str,
    config: &'static str,
    bare: &'static str,
}

const CLAUDE_SLOT: SlotVars = SlotVars {
    comm: "claude",
    config: "CLAUDE_CONFIG_DIR",
    bare: ".claude",
};

const CODEX_SLOT: SlotVars = SlotVars {
    comm: "codex",
    config: "CODEX_HOME",
    bare: ".codex",
};

/// The slot variables for an adapter id. `None` for a tool with no renewal
/// path - gemini and antigravity are never refreshed, so hold no slot.
fn slot_vars(tool: &str) -> Option<&'static SlotVars> {
    match tool {
        "claude-code" => Some(&CLAUDE_SLOT),
        "codex" => Some(&CODEX_SLOT),
        _ => None,
    }
}

/// The slot directory one process holds, read from its environment (`sep`
/// separated `KEY=VALUE`) and its process name.
///
/// The tool's own variable names the slot for ANY process carrying it: a
/// session exports it to every child, and the shim sets it before exec.
/// Without it, only a process that IS the tool is on a slot, and that slot is
/// `<HOME>/<bare>` - every process has a `HOME`, so honouring a stranger's
/// would pin the default slot permanently.
fn slot_dir_in(text: &str, sep: char, comm: &str, vars: &SlotVars) -> Option<std::path::PathBuf> {
    let mut home = None;
    for field in text.split([sep, '\n']) {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        if key == vars.config {
            return Some(std::path::PathBuf::from(value));
        }
        if key == "HOME" {
            home = Some(value);
        }
    }
    if comm != vars.comm {
        return None;
    }
    home.map(|h| std::path::Path::new(h).join(vars.bare))
}

/// pid -> process name, from one `ps` listing. Pairs with the `ps -E` listing,
/// which prints the environment but not the bare binary name.
fn ps_comm_by_pid() -> std::collections::HashMap<String, String> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-Ao", "pid=,comm="])
        .output()
    else {
        return std::collections::HashMap::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (pid, comm) = l.trim().split_once(char::is_whitespace)?;
            let name = comm.trim().rsplit('/').next()?.to_string();
            Some((pid.trim().to_string(), name))
        })
        .collect()
}

/// Is `tool` running right now with `dir` as its slot?
///
/// Renewing a credential retires the refresh token the running process holds in
/// memory, and its next renewal would then fail - the logout this project exists
/// to prevent. The environment of the live processes is what decides which
/// credential each one holds, so that is what is read. Directories are compared
/// as paths, so the spellings one shell or another produces are one slot.
pub fn config_dir_in_use(dir: &std::path::Path, tool: &str) -> bool {
    let Some(vars) = slot_vars(tool) else {
        return false;
    };
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            // Readable only for our own processes; a foreign session's slot is
            // unknowable, and treating that as in-use would refuse every
            // renewal on a shared machine.
            let Ok(bytes) = std::fs::read(e.path().join("environ")) else {
                continue;
            };
            let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            if slot_dir_in(&text, '\0', comm.trim(), vars).is_some_and(|d| d == dir) {
                return true;
            }
        }
        return false;
    }
    // macOS: `ps -E` prints each process's environment after its command, so the
    // slot a process actually holds is readable there too. An earlier version
    // fell back to "is any claude running at all", which refused to renew EVERY
    // account whenever one session was open - on the machine this is used from
    // that is always, so the whole feature was dead on arrival.
    let Ok(out) = std::process::Command::new("ps")
        .args(["-E", "-ww", "-o", "pid=,command="])
        .output()
    else {
        return false;
    };
    let comms = ps_comm_by_pid();
    String::from_utf8_lossy(&out.stdout).lines().any(|l| {
        let Some((pid, rest)) = l.trim().split_once(char::is_whitespace) else {
            return false;
        };
        let comm = comms.get(pid).map(String::as_str).unwrap_or_default();
        slot_dir_in(rest, ' ', comm, vars).is_some_and(|d| d == dir)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The slot a process holds is what decides whose credential a renewal would
    // pull out from under it. Reading "is the tool running at all" instead
    // refused every account whenever one session was open.
    #[test]
    fn a_slot_is_found_per_process_not_per_tool() {
        // macOS `ps -E`: command, then environment, space separated.
        let ps = "/usr/bin/claude PATH=/usr/bin CLAUDE_CONFIG_DIR=/home/me/.claude-work TERM=xterm";
        assert_eq!(
            slot_dir_in(ps, ' ', "claude", &CLAUDE_SLOT),
            Some("/home/me/.claude-work".into())
        );
        // Linux `/proc/<pid>/environ`: NUL separated.
        let environ = "PATH=/usr/bin\0CLAUDE_CONFIG_DIR=/home/me/.claude\0TERM=xterm\0";
        assert_eq!(
            slot_dir_in(environ, '\0', "claude", &CLAUDE_SLOT),
            Some("/home/me/.claude".into())
        );
        // Each tool reads its own variable, and a slot of one is never a slot
        // of the other - so a session must not be looked for under both.
        assert_eq!(slot_dir_in(environ, '\0', "claude", &CODEX_SLOT), None);
        assert_eq!(
            slot_dir_in(
                "CODEX_HOME=/home/me/.codex-alt\0",
                '\0',
                "codex",
                &CODEX_SLOT
            ),
            Some("/home/me/.codex-alt".into())
        );
        // No variable: the tool is on its default, anything else is on nothing.
        assert_eq!(
            slot_dir_in("HOME=/home/me\0", '\0', "claude", &CLAUDE_SLOT),
            Some("/home/me/.claude".into())
        );
        assert_eq!(
            slot_dir_in("HOME=/home/me\0", '\0', "bash", &CLAUDE_SLOT),
            None
        );
        assert_eq!(slot_dir_in("TERM=xterm", ' ', "claude", &CLAUDE_SLOT), None);
        // A named slot counts whoever carries it: a session exports it to every
        // child it spawns, and each of those holds the same credential.
        assert_eq!(
            slot_dir_in(
                "CLAUDE_CONFIG_DIR=/home/me/.claude-work\0",
                '\0',
                "node",
                &CLAUDE_SLOT
            ),
            Some("/home/me/.claude-work".into())
        );
    }

    // A tool swapdex never renews holds no slot to protect.
    #[test]
    fn only_the_renewable_tools_have_slot_vars() {
        assert!(slot_vars("claude-code").is_some());
        assert!(slot_vars("codex").is_some());
        assert!(slot_vars("gemini").is_none());
        assert!(slot_vars("antigravity").is_none());
    }

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
        proc_home(config, None)
    }

    fn proc_home(config: Option<&str>, home: Option<&str>) -> ClaudeProc {
        ClaudeProc {
            securestorage_dir: None,
            config_dir: config.map(str::to_string),
            home: home.map(str::to_string),
            env_read: true,
        }
    }

    fn env<'a>(config: Option<&'a str>, home: Option<&'a str>) -> SlotEnv<'a> {
        SlotEnv {
            securestorage: None,
            config,
            home,
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
            claude_switch_guard(None, None, None, std::slice::from_ref(&company)),
            GuardVerdict::Clear
        );
        // A plain `claude` (bare slot) running while we swap the bare slot: danger.
        let bare = proc(None);
        assert_eq!(
            claude_switch_guard(None, None, None, &[company.clone(), bare]),
            GuardVerdict::SameSlot
        );
        // Swapping the company slot while only a bare session runs: safe.
        assert_eq!(
            claude_switch_guard(None, Some("/home/u/.claude-company"), None, &[proc(None)]),
            GuardVerdict::Clear
        );
        // Swapping the company slot while a company session runs: danger.
        assert_eq!(
            claude_switch_guard(None, Some("/home/u/.claude-company"), None, &[company]),
            GuardVerdict::SameSlot
        );
    }

    /// The bare slot spelled out is still the bare slot.
    ///
    /// A session launched with `CLAUDE_CONFIG_DIR=$HOME/.claude` reads exactly
    /// the files a bare session reads. Comparing raw env strings calls that two
    /// slots and clears the switch that is about to overwrite them.
    #[test]
    fn guard_sees_the_bare_slot_spelled_out() {
        assert_eq!(
            claude_switch_guard(
                None,
                None,
                Some("/home/u"),
                &[proc_home(Some("/home/u/.claude"), None)]
            ),
            GuardVerdict::SameSlot
        );
        // The same collision from the other side: swapdex holds the explicit
        // spelling, the session is bare.
        assert_eq!(
            claude_switch_guard(
                None,
                Some("/home/u/.claude"),
                None,
                &[proc_home(None, Some("/home/u"))]
            ),
            GuardVerdict::SameSlot
        );
    }

    /// Two HOMEs share a Keychain but not a config dir.
    #[test]
    fn a_second_home_is_a_second_slot_only_where_the_files_are() {
        let a = env(None, Some("/home/a"));
        let b = env(None, Some("/home/b"));
        // Linux: the credential is `<config_dir>/.credentials.json`, so these
        // are two logins and flagging them would be a false alarm.
        assert!(!same_login_slot(&a, &b, false));
        // macOS: the bare Keychain service is one item for the OS user whatever
        // HOME says, so a switch really would clobber the other session.
        assert!(same_login_slot(&a, &b, true));
    }

    /// One directory spelled two ways is one directory.
    ///
    /// The credential a switch overwrites is a FILE under the config dir
    /// (`.credentials.json` on Linux) and the identity file `.claude.json` is
    /// one on every platform, so a trailing slash does not make a second slot -
    /// it makes the same file, and the guard has to say so.
    #[test]
    fn guard_sees_through_two_spellings_of_one_dir() {
        assert_eq!(
            claude_switch_guard(
                None,
                Some("/home/u/.claude-company"),
                None,
                &[proc(Some("/home/u/.claude-company/"))]
            ),
            GuardVerdict::SameSlot
        );
    }

    /// A bare session's HOME has to survive the parse.
    ///
    /// With no `CLAUDE_CONFIG_DIR` a session reads `<HOME>/.claude`. Drop HOME
    /// at the parser and every bare session reaches the guard as "no dirs at
    /// all", so the collision `guard_sees_the_bare_slot_spelled_out` describes
    /// can never fire on a real process.
    #[test]
    fn parsers_keep_the_home_a_bare_session_reads_from() {
        // Linux: the NUL-separated /proc/<pid>/environ blob.
        let bare = config_dirs_from_environ(b"PATH=/usr/bin\0HOME=/home/u\0");
        assert_eq!(bare.config_dir, None);
        assert_eq!(bare.home.as_deref(), Some("/home/u"));
        assert!(bare.env_read);

        // macOS: `ps eww`, which already reads HOME= as its proof the
        // environment was included - it just threw the value away.
        let bare_ps = config_dirs_from_ps("/opt/homebrew/bin/node claude HOME=/home/u");
        assert_eq!(bare_ps.config_dir, None);
        assert_eq!(bare_ps.home.as_deref(), Some("/home/u"));
        assert!(bare_ps.env_read);
    }

    #[test]
    fn environ_blob_yields_the_config_dirs() {
        // NUL-separated KEY=VALUE, as in /proc/<pid>/environ.
        let blob = b"PATH=/usr/bin\0CLAUDE_CONFIG_DIR=/home/u/.claude-company\0TERM=xterm\0";
        let p = config_dirs_from_environ(blob);
        assert_eq!(p.securestorage_dir, None);
        assert_eq!(p.config_dir.as_deref(), Some("/home/u/.claude-company"));
        // A bare-slot claude: no CONFIG_DIR set.
        let p2 = config_dirs_from_environ(b"PATH=/usr/bin\0HOME=/home/u\0");
        assert_eq!(p2.config_dir, None);
        // securestorage present.
        let p3 = config_dirs_from_environ(
            b"CLAUDE_SECURESTORAGE_CONFIG_DIR=/ss\0CLAUDE_CONFIG_DIR=/c\0",
        );
        assert_eq!(p3.securestorage_dir.as_deref(), Some("/ss"));
    }

    #[test]
    fn ps_line_parses_env_and_flags_visibility() {
        // `ps eww` line: command then env tokens. PATH= present -> env visible.
        let line = "/opt/homebrew/bin/node claude PATH=/usr/bin CLAUDE_CONFIG_DIR=/home/u/.claude-company TERM=xterm";
        let p = config_dirs_from_ps(line);
        assert_eq!(p.securestorage_dir, None);
        assert_eq!(p.config_dir.as_deref(), Some("/home/u/.claude-company"));
        assert!(
            p.env_read,
            "PATH= present means the environment was included"
        );
        // No env tokens at all (env NOT visible) -> must not be read as bare slot.
        let p2 = config_dirs_from_ps("/opt/homebrew/bin/node claude");
        assert_eq!(p2.config_dir, None);
        assert!(!p2.env_read, "no env visible -> caller must fail closed");
    }

    /// Copy `sleep` under `name` so the spawned process's `comm` is that name.
    #[cfg(target_os = "linux")]
    fn stub_binary(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let bin = root.join(name);
        std::fs::copy("/bin/sleep", &bin)
            .or_else(|_| std::fs::copy("/usr/bin/sleep", &bin))
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    /// The guard sees a real bare session, not just a synthetic one.
    ///
    /// `guard_sees_the_bare_slot_spelled_out` proves the comparison; this proves
    /// the wiring. A bare session sets no `CLAUDE_CONFIG_DIR`, so the only thing
    /// naming its login is `HOME` - drop it in the /proc parse and the guard is
    /// handed an empty environment and clears the switch that overwrites it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_bare_session_reaches_the_guard() {
        let dir = std::env::temp_dir().join(format!("swapdex_bare_test_{}", std::process::id()));
        let bin = stub_binary(&dir, "claude");

        // A HOME no other process on this machine can be holding, so finding it
        // proves we found OUR child.
        let home = dir.join("home").to_string_lossy().into_owned();
        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env("HOME", &home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .spawn()
            .unwrap();
        let mut mine = None;
        let mut last = Vec::new();
        for _ in 0..150 {
            last = running_claude_procs();
            if let Some(p) = last
                .iter()
                .find(|p| p.home.as_deref() == Some(home.as_str()))
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
            panic!("a bare session's HOME should survive the /proc parse; got {last:?}")
        });
        assert_eq!(mine.config_dir, None, "the child set no CLAUDE_CONFIG_DIR");
        // swapdex on the same HOME: switching would overwrite that session's login.
        assert_eq!(
            claude_switch_guard(None, None, Some(&home), std::slice::from_ref(&mine)),
            GuardVerdict::SameSlot
        );
        // A different HOME is a different `<HOME>/.claude` on Linux, so flagging
        // it would be a false alarm.
        assert_eq!(
            claude_switch_guard(None, None, Some("/home/somebody-else"), &[mine]),
            GuardVerdict::Clear
        );
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
        // A binary literally named `claude` so /proc/<pid>/comm == "claude".
        let bin = stub_binary(&dir, "claude");

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
            home: None,
            env_read: false,
        };
        // An unreadable session could be on this slot - do not clear it.
        assert_eq!(
            claude_switch_guard(None, None, None, std::slice::from_ref(&unknown)),
            GuardVerdict::Unknown
        );
        // No running claude at all: nothing to guard against.
        assert_eq!(
            claude_switch_guard(None, None, None, &[]),
            GuardVerdict::Clear
        );
        // A confirmed same-slot session outweighs an unknown one.
        assert_eq!(
            claude_switch_guard(None, None, None, &[unknown, proc(None)]),
            GuardVerdict::SameSlot
        );
    }

    /// A live Codex session holds its slot against a renewal.
    ///
    /// `refresh_codex_slot` documents that it never renews while Codex is
    /// running in that slot, because the session holds the refresh token the
    /// renewal retires. Codex is pinned by `CODEX_HOME`, and a Codex slot dir is
    /// never a Claude slot dir, so a reader that knows only `CLAUDE_CONFIG_DIR`
    /// never sees one and the guard it documents cannot fire.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_codex_session_holds_its_slot() {
        let root = std::env::temp_dir().join(format!("swapdex_cx_use_{}", std::process::id()));
        let bin = stub_binary(&root, "codex");

        // A slot dir no other process on this machine can be holding.
        let slot = root.join("slot");
        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env("CODEX_HOME", &slot)
            .spawn()
            .unwrap();
        let mut held = false;
        for _ in 0..150 {
            if config_dir_in_use(&slot, "codex") {
                held = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let other = config_dir_in_use(&root.join("other-slot"), "codex");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            held,
            "a running session pins its slot with CODEX_HOME; renewing it \
             retires the refresh token that session holds"
        );
        assert!(!other, "a slot no session is on is free to renew");
    }

    /// A session on the default slot holds it too.
    ///
    /// Nothing forces a session to name its slot: run the tool with no variable
    /// set and it reads `<HOME>/.claude`, which `swapdex adopt` accepts as a
    /// slot like any other. Such a session holds the refresh token that a
    /// renewal of that slot retires, so it has to be seen.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_bare_session_holds_its_default_slot() {
        let root = std::env::temp_dir().join(format!("swapdex_bare_use_{}", std::process::id()));
        let bin = stub_binary(&root, "claude");
        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env("HOME", &root)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .spawn()
            .unwrap();
        let environ = format!("/proc/{}/environ", child.id());
        for _ in 0..300 {
            if std::fs::read(&environ).is_ok_and(|b| !b.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let held = config_dir_in_use(&root.join(".claude"), "claude-code");
        let other = config_dir_in_use(&root.join(".claude-work"), "claude-code");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);

        assert!(held, "a session with no variable set is on <HOME>/.claude");
        assert!(!other, "and only on that one");
    }

    /// One directory spelled two ways is one slot.
    ///
    /// The variable carries whatever the shell put there - a trailing slash, a
    /// doubled separator - while the slot is stored as a clean path. Comparing
    /// the two as text reads one live session as being on some other slot, and
    /// the renewal goes ahead.
    #[cfg(target_os = "linux")]
    #[test]
    fn two_spellings_of_one_slot_are_one_slot() {
        let root = std::env::temp_dir().join(format!("swapdex_spell_use_{}", std::process::id()));
        let bin = stub_binary(&root, "claude");
        let slot = root.join("slot");
        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", format!("{}//", slot.display()))
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .spawn()
            .unwrap();
        let environ = format!("/proc/{}/environ", child.id());
        for _ in 0..300 {
            if std::fs::read(&environ).is_ok_and(|b| !b.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let held = config_dir_in_use(&slot, "claude-code");
        let other = config_dir_in_use(&root.join("slot2"), "claude-code");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);

        assert!(held, "`<slot>//` and `<slot>` are the same directory");
        assert!(!other, "a neighbouring slot is still free");
    }
}
