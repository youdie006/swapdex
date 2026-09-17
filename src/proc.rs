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

/// One exact native CLI process's on-disk OAuth source.
///
/// This is intentionally narrower than [`running_config_dirs`]. The refresh
/// guard includes children that inherit a slot variable because they can still
/// hold a retired token in memory. A native login authority must itself have
/// the exact `claude` or `codex` process name, and must not be using an
/// alternate environment credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeLoginProcess {
    pub(crate) source_dir: std::path::PathBuf,
    pub(crate) identity_path: std::path::PathBuf,
    /// Claude's raw secure-storage key. `None` is the bare Keychain service;
    /// `Some` is hashed by the adapter. Unused for Codex.
    pub(crate) claude_keychain_key: Option<String>,
    /// Only native releases whose lock and reread protocol has been verified.
    pub(crate) supports_refresh_locks: bool,
}

#[derive(Default)]
struct NativeProcessEnv {
    home: Option<String>,
    claude_config: Option<String>,
    claude_securestorage: Option<String>,
    codex_home: Option<String>,
    alternate_credential: bool,
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn native_process_env(text: &str, sep: char, tool: &str) -> NativeProcessEnv {
    let mut env = NativeProcessEnv::default();
    for field in text.split([sep, '\n']) {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "HOME" => env.home = Some(value.to_string()),
            "CLAUDE_CONFIG_DIR" => env.claude_config = Some(value.to_string()),
            "CLAUDE_SECURESTORAGE_CONFIG_DIR" => env.claude_securestorage = Some(value.to_string()),
            "CODEX_HOME" => env.codex_home = Some(value.to_string()),
            // These bypass the native OAuth store. Seeing a matching file next
            // to such a process does not prove that the process owns it.
            "ANTHROPIC_API_KEY" | "ANTHROPIC_AUTH_TOKEN" | "CLAUDE_CODE_OAUTH_TOKEN"
                if tool == "claude-code" && !value.is_empty() =>
            {
                env.alternate_credential = true
            }
            "OPENAI_API_KEY" | "CODEX_API_KEY" if tool == "codex" && !value.is_empty() => {
                env.alternate_credential = true
            }
            _ => {}
        }
    }
    env
}

fn native_process_from_env(
    paths: &crate::paths::Paths,
    tool: &str,
    comm: &str,
    text: &str,
    sep: char,
) -> Option<NativeLoginProcess> {
    let vars = slot_vars(tool)?;
    if comm != vars.comm {
        return None;
    }
    let env = native_process_env(text, sep, tool);
    if env.alternate_credential {
        return None;
    }
    let home = nonempty(env.home.as_deref()).map(std::path::PathBuf::from);
    let (source_dir, identity_path, claude_keychain_key) = match tool {
        "claude-code" => {
            let config = nonempty(env.claude_config.as_deref());
            let config_dir = config
                .map(std::path::PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".claude")))?;
            let identity = match config {
                Some(_) => config_dir.join(".claude.json"),
                None => home.as_ref()?.join(".claude.json"),
            };
            let source = match env.claude_securestorage.as_deref() {
                Some("") => home.as_ref()?.join(".claude"),
                Some(storage) => std::path::PathBuf::from(storage),
                None => config_dir,
            };
            let key = slot_key(
                env.claude_securestorage.as_deref(),
                env.claude_config.as_deref(),
            );
            (source, identity, key)
        }
        "codex" => {
            let source = nonempty(env.codex_home.as_deref())
                .map(std::path::PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".codex")))?;
            let identity = source.join("auth.json");
            (source, identity, None)
        }
        _ => return None,
    };
    if !native_path_allowed(paths, &source_dir) || !native_path_allowed(paths, &identity_path) {
        return None;
    }
    Some(NativeLoginProcess {
        source_dir,
        identity_path,
        claude_keychain_key,
        supports_refresh_locks: paths.sandboxed()
            && text
                .split([sep, '\n'])
                .any(|field| field == "SWAPDEX_TEST_NATIVE_REFRESH_LOCKS=1"),
    })
}

fn known_refresh_lock_executable(path: &std::path::Path) -> bool {
    let verified_version = |version: Option<&str>| {
        matches!(version, Some("2.1.271" | "2.1.272" | "2.1.273" | "2.1.274"))
    };
    // Standalone native installs retain their release number as the basename.
    if verified_version(path.file_name().and_then(|name| name.to_str()))
        && path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|p| p == "versions")
    {
        return true;
    }
    // The native npm package uses bin/claude.exe on macOS as well as Linux.
    // Node wrappers and unrelated executables never inherit this capability.
    let Some(package) = path
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "bin"))
        .and_then(|p| p.parent())
    else {
        return false;
    };
    if path.file_name().is_none_or(|name| name != "claude.exe")
        || package.file_name().is_none_or(|name| name != "claude-code")
        || package
            .parent()
            .and_then(|p| p.file_name())
            .is_none_or(|name| name != "@anthropic-ai")
    {
        return false;
    }
    let Some(manifest) = crate::atomic::read_regular(&package.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return false;
    };
    manifest["name"] == "@anthropic-ai/claude-code"
        && manifest["bin"]["claude"] == "bin/claude.exe"
        && verified_version(manifest["version"].as_str())
}

#[cfg(target_os = "macos")]
fn macos_refresh_locks_supported(pid: &str) -> bool {
    let Ok(pid) = pid.parse::<i32>() else {
        return false;
    };
    let mut bytes = vec![0u8; 4096];
    let count = unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if count <= 0 {
        return false;
    }
    let path = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    std::str::from_utf8(path)
        .ok()
        .is_some_and(|path| known_refresh_lock_executable(std::path::Path::new(path)))
}

#[cfg(test)]
mod secure_storage_root_tests {
    use super::*;

    #[test]
    fn verified_npm_native_install_participates_but_unknown_versions_do_not() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("node_modules/@anthropic-ai/claude-code");
        std::fs::create_dir_all(package.join("bin")).unwrap();
        let binary = package.join("bin/claude.exe");
        std::fs::write(&binary, b"fixture-native").unwrap();
        let manifest = package.join("package.json");
        std::fs::write(&manifest, br#"{"name":"@anthropic-ai/claude-code","version":"2.1.274","bin":{"claude":"bin/claude.exe"}}"#).unwrap();
        assert!(known_refresh_lock_executable(&binary));
        std::fs::write(&manifest, br#"{"name":"@anthropic-ai/claude-code","version":"2.1.999","bin":{"claude":"bin/claude.exe"}}"#).unwrap();
        assert!(!known_refresh_lock_executable(&binary));
        std::fs::write(
            &manifest,
            br#"{"name":"another-package","version":"2.1.274","bin":{"claude":"bin/claude.exe"}}"#,
        )
        .unwrap();
        assert!(!known_refresh_lock_executable(&binary));
        assert!(!known_refresh_lock_executable(&root.path().join("2.1.274")));
    }

    #[test]
    fn explicit_secure_storage_keeps_identity_in_the_session_config() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(root.path());
        let config = root.path().join("session-config");
        let storage = root.path().join("shared-auth");
        let text = format!(
            "HOME={}\0CLAUDE_CONFIG_DIR={}\0CLAUDE_SECURESTORAGE_CONFIG_DIR={}\0",
            root.path().display(),
            config.display(),
            storage.display()
        );
        let process =
            native_process_from_env(&paths, "claude-code", "claude", &text, '\0').unwrap();
        assert_eq!(process.source_dir, storage);
        assert_eq!(process.identity_path, config.join(".claude.json"));
    }

    #[test]
    fn explicit_empty_secure_storage_uses_default_auth_with_slot_identity() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(root.path());
        let config = root.path().join("session-config");
        let text = format!(
            "HOME={}\0CLAUDE_CONFIG_DIR={}\0CLAUDE_SECURESTORAGE_CONFIG_DIR=\0",
            root.path().display(),
            config.display()
        );
        let process =
            native_process_from_env(&paths, "claude-code", "claude", &text, '\0').unwrap();
        assert_eq!(process.source_dir, root.path().join(".claude"));
        assert_eq!(process.identity_path, config.join(".claude.json"));
        assert_eq!(process.claude_keychain_key, None);
    }
}

/// A sandbox may only inspect native sources inside its own rooted HOME.
///
/// The lexical check happens before `canonicalize`, so a real user's path is
/// rejected without even resolving it. Canonical containment then catches a
/// directory symlink that lexically starts inside the sandbox but escapes it.
pub(crate) fn native_path_allowed(paths: &crate::paths::Paths, path: &std::path::Path) -> bool {
    if !paths.sandboxed() {
        return path.is_absolute();
    }
    let root = paths.home();
    if !path.is_absolute()
        || !path.starts_with(root)
        || path.strip_prefix(root).ok().is_none_or(|relative| {
            relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        })
    {
        return false;
    }
    let Some(existing) = path.ancestors().find(|candidate| candidate.exists()) else {
        return false;
    };
    let (Ok(real_root), Ok(real_existing)) =
        (std::fs::canonicalize(root), std::fs::canonicalize(existing))
    else {
        return false;
    };
    real_existing.starts_with(real_root)
}

/// Locate the NUL-delimited environment inside a macOS `KERN_PROCARGS2` blob.
///
/// Layout: native-endian `argc`, executable path, alignment NULs, `argc` argv
/// strings, then environment `KEY=VALUE` strings. Returning the original byte
/// slice keeps spaces in paths intact and never formats or logs the environment.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn kern_procargs2_environ(blob: &[u8]) -> Option<&[u8]> {
    let argc_bytes: [u8; std::mem::size_of::<i32>()] =
        blob.get(..std::mem::size_of::<i32>())?.try_into().ok()?;
    let argc = i32::from_ne_bytes(argc_bytes);
    if !(0..=1_000_000).contains(&argc) {
        return None;
    }
    let mut position = std::mem::size_of::<i32>();

    // Executable path.
    position += blob.get(position..)?.iter().position(|byte| *byte == 0)? + 1;
    // Kernel alignment padding before argv[0].
    while blob.get(position) == Some(&0) {
        position += 1;
    }
    // Exactly argc NUL-terminated arguments. Spaces are ordinary bytes here.
    for _ in 0..argc {
        position += blob.get(position..)?.iter().position(|byte| *byte == 0)? + 1;
    }
    Some(blob.get(position..).unwrap_or_default())
}

#[cfg(target_os = "macos")]
fn macos_process_environ(pid: i32) -> Option<Vec<u8>> {
    const MAX_PROCARGS: usize = 4 * 1024 * 1024;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0usize;
    let size_status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if size_status != 0 || size == 0 || size > MAX_PROCARGS {
        return None;
    }
    let mut blob = vec![0u8; size];
    let read_status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            blob.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read_status != 0 || size > blob.len() {
        return None;
    }
    blob.truncate(size);
    kern_procargs2_environ(&blob).map(ToOwned::to_owned)
}

/// Exact native Claude/Codex processes whose OAuth source can be identified.
///
/// This accessor is read-only. Unreadable environments, alternate credentials,
/// relative paths and foreign sandbox paths are skipped conservatively.
pub(crate) fn running_native_login_processes(
    paths: &crate::paths::Paths,
    tool: &str,
) -> Vec<NativeLoginProcess> {
    let Some(vars) = slot_vars(tool) else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            if comm.trim() != vars.comm {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            if let Some(mut process) =
                native_process_from_env(paths, tool, comm.trim(), &text, '\0')
            {
                process.supports_refresh_locks |= std::fs::read_link(entry.path().join("exe"))
                    .is_ok_and(|path| known_refresh_lock_executable(&path));
                processes.push(process);
            }
        }
        return processes;
    }

    let comms = ps_comm_by_pid();
    #[cfg(target_os = "macos")]
    {
        for (pid, comm) in comms {
            if comm != vars.comm {
                continue;
            }
            let Some(bytes) = pid.parse::<i32>().ok().and_then(macos_process_environ) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            if let Some(mut process) = native_process_from_env(paths, tool, &comm, text, '\0') {
                process.supports_refresh_locks |= macos_refresh_locks_supported(&pid);
                processes.push(process);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        for (pid, comm) in comms {
            if comm != vars.comm {
                continue;
            }
            let Ok(output) = std::process::Command::new("ps")
                .args(["eww", "-o", "command=", "-p", &pid])
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(process) = native_process_from_env(paths, tool, &comm, &text, ' ') {
                processes.push(process);
            }
        }
    }
    processes
}

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

/// Every config directory held by a running process for `tool`.
///
/// Renewing a credential retires the refresh token the running process holds in
/// memory, and its next renewal would then fail - the logout this project exists
/// to prevent. The environment of the live processes is what decides which
/// credential each one holds, so that is what is returned to refresh safety.
pub fn running_config_dirs(tool: &str) -> Vec<std::path::PathBuf> {
    let Some(vars) = slot_vars(tool) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
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
            if let Some(dir) = slot_dir_in(&text, '\0', comm.trim(), vars) {
                dirs.push(dir);
            }
        }
        return dirs;
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
        return dirs;
    };
    let comms = ps_comm_by_pid();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let l = line;
        let Some((pid, rest)) = l.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let comm = comms.get(pid).map(String::as_str).unwrap_or_default();
        if let Some(dir) = slot_dir_in(rest, ' ', comm, vars) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Whether one exact config directory is held by a running tool process.
pub fn config_dir_in_use(dir: &std::path::Path, tool: &str) -> bool {
    running_config_dirs(tool).into_iter().any(|d| d == dir)
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

    /// Link `sleep` under `name` so the spawned process's `comm` is that name.
    #[cfg(target_os = "linux")]
    fn stub_binary(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let bin = root.join(name);
        // Keep the requested comm without executing a freshly written file,
        // which can produce ETXTBSY while concurrent tests spawn children.
        let sleep = if std::path::Path::new("/bin/sleep").exists() {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        std::os::unix::fs::symlink(sleep, &bin).unwrap();
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

    #[test]
    fn kern_procargs2_keeps_native_paths_with_spaces_and_securestorage_precedence() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(root.path());
        let config = root
            .path()
            .join("Library/Application Support/swapdex/claude-personal");
        let secure = root
            .path()
            .join("Library/Application Support/Claude Secure Storage");
        std::fs::create_dir_all(&config).unwrap();

        let mut blob = Vec::new();
        blob.extend_from_slice(&2i32.to_ne_bytes());
        blob.extend_from_slice(b"/opt/homebrew/bin/claude\0\0\0");
        blob.extend_from_slice(b"claude\0--resume with spaces\0");
        blob.extend_from_slice(format!("HOME={}\0", root.path().display()).as_bytes());
        blob.extend_from_slice(format!("CLAUDE_CONFIG_DIR={}\0", config.display()).as_bytes());
        blob.extend_from_slice(
            format!("CLAUDE_SECURESTORAGE_CONFIG_DIR={}\0", secure.display()).as_bytes(),
        );

        let environ = kern_procargs2_environ(&blob).expect("valid KERN_PROCARGS2 payload");
        let text = std::str::from_utf8(environ).unwrap();
        let process = native_process_from_env(&paths, "claude-code", "claude", text, '\0')
            .expect("path with spaces remains one environment value");
        assert_eq!(process.source_dir, secure);
        assert_eq!(process.identity_path, config.join(".claude.json"));
        assert_eq!(
            process.claude_keychain_key.as_deref(),
            secure.to_str(),
            "securestorage overrides CLAUDE_CONFIG_DIR for the Keychain service"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sysctl_reads_only_the_spawned_child_environment_with_spaces() {
        const CHILD_MARKER: &str = "SWAPDEX_NATIVE_ENV_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
            return;
        }
        struct ReapedChild(std::process::Child);

        impl Drop for ReapedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("Home With Spaces");
        let config = home.join("Library/Application Support/Claude Work");
        std::fs::create_dir_all(&config).unwrap();
        let paths = crate::paths::Paths::rooted(&home);
        // macOS can omit the environment of protected system binaries such as
        // /bin/sleep. Re-exec this ordinary test executable as the owned fixture.
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proc::tests::macos_sysctl_reads_only_the_spawned_child_environment_with_spaces",
            ])
            .env_clear()
            .env(CHILD_MARKER, "1")
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let child = ReapedChild(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let environ = loop {
            if let Some(environ) = macos_process_environ(child.0.id() as i32) {
                break environ;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the spawned child's KERN_PROCARGS2 environment remained unreadable"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let text = std::str::from_utf8(&environ).unwrap();
        let parsed = native_process_env(text, '\0', "claude-code");
        assert_eq!(parsed.home.as_deref(), home.to_str());
        assert_eq!(parsed.claude_config.as_deref(), config.to_str());

        let process = native_process_from_env(&paths, "claude-code", "claude", text, '\0')
            .expect("the owned child maps to its exact native source");
        assert_eq!(process.source_dir, config);
        assert_eq!(process.identity_path, config.join(".claude.json"));
    }
}
