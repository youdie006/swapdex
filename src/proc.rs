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

/// Does a process for `tool` (an adapter id) appear to be running? Best-effort:
/// exact-match the tool's binary name against the collected process names.
pub fn tool_running(tool: &str, comms: &[String]) -> bool {
    let want = match tool {
        "claude-code" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
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
}
