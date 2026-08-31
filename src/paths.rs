//! One override-first path resolver per tool. Every canonical credential path
//! goes through here so tests can redirect to a temp tree and never touch a
//! real login. Precedence: explicit root (tests) > tool env var > home dir.

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Paths {
    home: PathBuf,       // for ~/.claude.json (sibling of ~/.claude)
    claude_dir: PathBuf, // ~/.claude or $CLAUDE_CONFIG_DIR
    codex_dir: PathBuf,  // ~/.codex or $CODEX_HOME
    gemini_dir: PathBuf, // ~/.gemini
    data: PathBuf,       // ~/.local/share/swapdex
    /// These paths are a sandbox, not the machine's real ones. Anything with an
    /// effect that OUTLIVES the process has to ask: a detached proxy started for
    /// a temporary store keeps its port and answers for a directory that is
    /// deleted moments later.
    sandboxed: bool,
}

impl Paths {
    /// Test constructor: everything under one temp root, so no test can touch a
    /// real credential. `.claude.json` sits at <root>/.claude.json (home root),
    /// matching the real sibling layout.
    pub fn rooted(root: &Path) -> Paths {
        Paths {
            home: root.to_path_buf(),
            claude_dir: root.join(".claude"),
            codex_dir: root.join(".codex"),
            gemini_dir: root.join(".gemini"),
            data: root.join(".local/share/swapdex"),
            sandboxed: true,
        }
    }

    /// Are these a redirected (test or SWAPDEX_ROOT) tree rather than the real
    /// one? Checked before starting anything that outlives this process.
    /// The same paths, but with ONE tool config dir pointed somewhere else.
    ///
    /// The sign-in key lands a login in an account own slot directory, while
    /// every capture reads whatever dir was resolved at startup - so the saved
    /// copy could never be refreshed from the login just made, and the "stale"
    /// marker had nothing that could clear it. The store is deliberately left
    /// alone: only where the TOOL credential is read from moves.
    ///
    /// An environment variable would not do: the sandbox used by tests ignores
    /// those by design, so the one check that proves this works could never run.
    pub fn with_tool_dir(&self, tool: &str, dir: &Path) -> Paths {
        let mut p = self.clone();
        match tool {
            "claude-code" => p.claude_dir = dir.to_path_buf(),
            "codex" => p.codex_dir = dir.to_path_buf(),
            "gemini" => p.gemini_dir = dir.to_path_buf(),
            _ => {}
        }
        p
    }

    pub fn sandboxed(&self) -> bool {
        self.sandboxed
    }

    /// The real resolver: honors CLAUDE_CONFIG_DIR / CODEX_HOME, else home dir.
    /// SWAPDEX_ROOT redirects everything under one dir (dev/test override).
    pub fn resolve() -> anyhow::Result<Paths> {
        use anyhow::Context;
        if let Some(root) = std::env::var_os("SWAPDEX_ROOT") {
            return Ok(Paths::rooted(Path::new(&root)));
        }
        let home = dirs::home_dir().context("cannot determine home dir")?;
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let codex_dir = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let data = dirs::data_dir()
            .context("cannot determine data dir")?
            .join("swapdex");
        let gemini_dir = home.join(".gemini");
        Ok(Paths {
            home,
            claude_dir,
            codex_dir,
            gemini_dir,
            data,
            sandboxed: false,
        })
    }

    pub fn claude_credentials(&self) -> PathBuf {
        self.claude_dir.join(".credentials.json")
    }
    pub fn claude_config_json(&self) -> PathBuf {
        self.home.join(".claude.json")
    }
    pub fn codex_auth(&self) -> PathBuf {
        self.codex_dir.join("auth.json")
    }
    pub fn gemini_oauth(&self) -> PathBuf {
        self.gemini_dir.join("oauth_creds.json")
    }
    pub fn gemini_accounts(&self) -> PathBuf {
        self.gemini_dir.join("google_accounts.json")
    }
    /// Antigravity CLI keeps its own token under the gemini dir.
    pub fn antigravity_token(&self) -> PathBuf {
        self.gemini_dir
            .join("antigravity-cli")
            .join("antigravity-oauth-token")
    }
    /// Where a backgrounded proxy writes what it says.
    ///
    /// One per tool. A proxy started by the shim used to discard its output, so
    /// on a machine where that is how it starts there was no record of which
    /// account served which turn - the single question these lines exist to
    /// answer, and three wrong diagnoses came out of not having it.
    pub fn proxy_log(&self, tool: &str) -> PathBuf {
        let name = match tool {
            "codex" => "proxy-codex.log",
            "gemini" => "proxy-gemini.log",
            "antigravity" => "proxy-antigravity.log",
            _ => "proxy-claude.log",
        };
        self.data.join("logs").join(name)
    }

    pub fn store_dir(&self) -> PathBuf {
        self.data.clone()
    }
    /// The bare Claude config dir (`~/.claude`) - the source of shared,
    /// account-agnostic config (settings, global memory) linked into new slots.
    pub fn claude_dir(&self) -> &Path {
        &self.claude_dir
    }
    /// The bare Codex home (`~/.codex`, or `$CODEX_HOME`) - the source of
    /// shared, account-agnostic config linked into new Codex slots.
    pub fn codex_dir(&self) -> &Path {
        &self.codex_dir
    }
    /// Sibling `~/.claude-*` config dirs a user already runs via aliases -
    /// adoptable as slots during onboarding. Excludes the bare `~/.claude`.
    /// Best-effort; empty on failure.
    pub fn discover_claude_config_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.home) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with(".claude-") && e.path().is_dir() {
                    out.push(e.path());
                }
            }
        }
        out.sort();
        // The bare `~/.claude` LAST, and only when it exists. Leaving it out made
        // the account everyone starts from the one account swapdex could not
        // switch back to - and every conversation begun before the first switch
        // lives in it, so they became unreachable by a plain `claude -r`.
        if self.claude_dir.is_dir() {
            out.push(self.claude_dir.clone());
        }
        out
    }
    /// Claude Code's session transcripts (for local, no-network usage reads).
    pub fn claude_projects(&self) -> PathBuf {
        self.claude_dir.join("projects")
    }
    /// Codex's session transcripts.
    pub fn codex_sessions(&self) -> PathBuf {
        self.codex_dir.join("sessions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooted_redirects_every_path_under_the_temp_root() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::rooted(dir.path());
        for path in [
            p.claude_credentials(),
            p.claude_config_json(),
            p.codex_auth(),
            p.store_dir(),
        ] {
            assert!(path.starts_with(dir.path()), "{path:?} escaped the root");
        }
        // .claude.json is a sibling of .claude/, at the home root.
        assert_eq!(p.claude_config_json(), dir.path().join(".claude.json"));
        assert!(p
            .claude_credentials()
            .starts_with(dir.path().join(".claude")));
    }
}

#[cfg(test)]
mod log_path_tests {
    use super::*;

    /// A proxy started in the background used to throw its output away, so on a
    /// machine where the shim starts it there was no record of which account
    /// served which turn - the one question these logs exist to answer. Three
    /// wrong diagnoses came out of that silence.
    #[test]
    fn a_backgrounded_proxy_has_somewhere_to_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::rooted(dir.path());
        let a = p.proxy_log("claude-code");
        let b = p.proxy_log("codex");
        assert_ne!(a, b, "each tool keeps its own log");
        // Under the store, so it travels with the rest of swapdex's state.
        assert!(a.starts_with(p.store_dir()), "{a:?}");
        assert!(a.to_string_lossy().contains("log"), "named as a log: {a:?}");
    }
}

#[cfg(test)]
mod pointed_at_slot_tests {
    use super::*;

    /// Capturing must be able to read ONE named slot, not just the default home.
    ///
    /// The sign-in key lands a login in the account's own slot directory, while
    /// every capture path reads whatever dir `Paths` resolved at startup. So the
    /// saved copy could never be refreshed from the login just made, and the
    /// stale marker had nothing that could clear it. Pointing an existing Paths
    /// at one slot is what makes that capture possible - and testable, which
    /// matters because a SWAPDEX_ROOT fixture deliberately ignores the env vars
    /// the real resolver honours.
    #[test]
    fn a_paths_can_be_pointed_at_one_slot() {
        let base = Paths::rooted(std::path::Path::new("/tmp/swapdex-pointed"));
        let slot = std::path::Path::new("/tmp/swapdex-pointed/slotdir");
        let at = base.with_tool_dir("claude-code", slot);
        assert_eq!(at.claude_dir(), slot, "claude reads the slot");
        assert_eq!(
            at.codex_dir(),
            base.codex_dir(),
            "other tools are untouched"
        );
        assert_eq!(at.store_dir(), base.store_dir(), "the store never moves");
        let cx = base.with_tool_dir("codex", slot);
        assert_eq!(cx.codex_dir(), slot);
        assert_eq!(cx.claude_dir(), base.claude_dir());
    }
}
