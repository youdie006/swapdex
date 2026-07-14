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
        }
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
    pub fn store_dir(&self) -> PathBuf {
        self.data.clone()
    }
    /// The bare Claude config dir (`~/.claude`) - the source of shared,
    /// account-agnostic config (settings, global memory) linked into new slots.
    pub fn claude_dir(&self) -> &Path {
        &self.claude_dir
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
