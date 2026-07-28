//! The `claude` shim: a tiny launcher placed on the user's PATH (ahead of the
//! real `claude`) that reads swapdex's default-account pointer and runs the real
//! `claude` in that account's slot. This is what makes a plain `claude` follow
//! `swapdex use`. No credential is ever moved - the shim only sets
//! `CLAUDE_CONFIG_DIR`.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where swapdex installs the shim: `<store_dir>/bin/claude`.
pub fn shim_path(paths: &Paths) -> PathBuf {
    shim_path_for(paths, "claude-code")
}

/// Where a given tool's shim lives: `<store_dir>/bin/<binary>`.
pub fn shim_path_for(paths: &Paths, tool: &str) -> PathBuf {
    let bin = match tool {
        "codex" => "codex",
        _ => "claude",
    };
    paths.store_dir().join("bin").join(bin)
}

/// Single-quote a path for safe embedding in the /bin/sh shim script.
fn sh_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shim script body. The default-account pointer only fills in when nothing
/// has already chosen a config dir: an explicit `CLAUDE_CONFIG_DIR` (what
/// `swapdex run <account>` sets, or what a user exports by hand) is a decision
/// already made, and overriding it meant every account opened as the default one.
///
/// It also gets proxy mode for free: the shim asks `swapdex proxy --ensure`,
/// which prints the port of a running proxy and starts one in the background if
/// there is none. So mid-session account switching works without the user
/// launching or exporting anything, and a proxy that cannot start is not an
/// error - the shim just runs Claude directly.
pub fn shim_script(pointer: &Path, real_claude: &Path, swapdex: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # swapdex claude shim - launch claude in the default account's slot.\n\
         # Managed by swapdex; re-created by `swapdex shim`.\n\
         # Ask swapdex for a live proxy (it starts one if needed and prints the\n\
         # port); silence and a non-zero status mean \"run without one\".\n\
         port=$({sx} proxy --ensure 2>/dev/null)\n\
         if [ -n \"$port\" ]; then\n\
         \tANTHROPIC_BASE_URL=\"http://127.0.0.1:$port\"\n\
         \texport ANTHROPIC_BASE_URL\n\
         fi\n\
         if [ -z \"$CLAUDE_CONFIG_DIR\" ]; then\n\
         \tdir=$(cat {ptr} 2>/dev/null)\n\
         \tif [ -n \"$dir\" ]; then\n\
         \t\tCLAUDE_CONFIG_DIR=\"$dir\"\n\
         \t\texport CLAUDE_CONFIG_DIR\n\
         \tfi\n\
         fi\n\
         exec {real} \"$@\"\n",
        sx = sh_quote(swapdex),
        ptr = sh_quote(pointer),
        real = sh_quote(real_claude),
    )
}

/// The `codex` shim. Same shape as Claude's - fill the tool's home from the
/// default pointer, and only when nothing has already chosen one - but with
/// Codex's own variable and pointer. It never mentions Claude's: one tool's shim
/// moving the other tool's account is exactly what the per-tool split prevents.
pub fn codex_shim_script(pointer: &Path, real_codex: &Path, _swapdex: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # swapdex codex shim - launch codex in the default account's slot.\n\
         # Managed by swapdex; re-created by `swapdex shim`.\n\
         if [ -z \"$CODEX_HOME\" ]; then\n\
         \tdir=$(cat {ptr} 2>/dev/null)\n\
         \tif [ -n \"$dir\" ]; then\n\
         \t\tCODEX_HOME=\"$dir\"\n\
         \t\texport CODEX_HOME\n\
         \tfi\n\
         fi\n\
         exec {real} \"$@\"\n",
        ptr = sh_quote(pointer),
        real = sh_quote(real_codex),
    )
}

/// Where a running proxy announces itself: `<store_dir>/proxy`, holding
/// "<pid> <port>". Written on start, removed on exit.
pub fn proxy_marker(paths: &Paths) -> PathBuf {
    paths.store_dir().join("proxy")
}

/// A marker line the generated shim carries, so we can recognize (and never
/// re-exec) our own shim regardless of how its dir is spelled on PATH.
const SHIM_MARKER: &str = "swapdex claude shim";

/// The same, for the codex shim.
const SHIM_MARKER_CODEX: &str = "swapdex codex shim";

/// True if `path` is one of swapdex's own `claude` shims (by content), not the
/// real binary. Robust against path-spelling: a `~`, symlink, or relative PATH
/// entry that resolves to the shim dir would slip past a plain path comparison.
fn is_our_shim(path: &Path) -> bool {
    // The shim is a tiny text script; read only its head.
    let mut buf = [0u8; 256];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let n = f.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.contains(SHIM_MARKER) || head.contains(SHIM_MARKER_CODEX)
}

/// What a plain `claude` typed in THIS environment resolves to: the first
/// `claude` file on PATH. The bool says whether that is swapdex's own shim (by
/// content marker, robust to path spelling). `None` when PATH has no `claude`.
/// Feeds doctor's engagement check - an installed shim that PATH never reaches
/// LOOKS set up while `swapdex use` silently does nothing.
pub(crate) fn resolved_claude() -> Option<(PathBuf, bool)> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("claude");
        if cand.is_file() {
            let ours = is_our_shim(&cand);
            return Some((cand, ours));
        }
    }
    None
}

/// The first `claude` on PATH that is NOT swapdex's own shim - the real one the
/// shim should exec. Skips the shim dir AND any `claude` that is itself one of
/// our shims (so re-running `swapdex shim` can never bake a self-reference).
fn find_real_claude(shim_dir: &Path) -> Option<PathBuf> {
    find_real(shim_dir, "claude")
}

/// The real `bin` on PATH, skipping our own shim dir and any shim we wrote.
fn find_real(shim_dir: &Path, bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir == shim_dir {
            continue;
        }
        let cand = dir.join(bin);
        if cand.is_file() && !is_our_shim(&cand) {
            return Some(cand);
        }
    }
    None
}

/// The line a shell profile needs so the shim is found first.
fn path_line(shim_dir: &Path) -> String {
    format!("export PATH=\"{}:$PATH\"", shim_dir.display())
}

/// A marker so the block can be recognised, skipped on a re-run, and found by a
/// human wondering what edited their profile.
const PROFILE_MARKER: &str = "# added by swapdex (claude shim)";

/// The shell profile to teach: the one belonging to $SHELL, since that is the
/// shell the user actually gets. Returns `None` for a shell we should not guess at
/// (fish and friends keep PATH somewhere else entirely).
fn shell_profile() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let name = shell.rsplit('/').next().unwrap_or("");
    match name {
        "zsh" => Some(home.join(".zshrc")),
        "bash" => {
            // Login shells on macOS read .bash_profile; .bashrc elsewhere. Prefer
            // whichever already exists so the line lands where it is read.
            let bp = home.join(".bash_profile");
            if bp.exists() {
                Some(bp)
            } else {
                Some(home.join(".bashrc"))
            }
        }
        _ => None,
    }
}

/// Is the shim dir already on PATH for this session?
fn already_on_path(shim_dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == shim_dir))
        .unwrap_or(false)
}

/// What `ensure_on_path` did, so the caller can say the right thing.
pub enum PathSetup {
    /// Already reachable; nothing to do.
    AlreadyThere,
    /// The line was appended to this profile; a new shell will pick it up.
    Added(PathBuf),
    /// We will not guess for this shell - the caller prints the line to add.
    Manual,
}

/// Put the shim dir on PATH by editing the user's shell profile, because leaving
/// this to the user means the shim silently does nothing: it is installed, PATH
/// never reaches it, and `swapdex use` appears to work while changing nothing.
/// Idempotent - a profile that already carries the marker is left alone.
pub fn ensure_on_path(shim_dir: &Path) -> Result<PathSetup> {
    if already_on_path(shim_dir) {
        return Ok(PathSetup::AlreadyThere);
    }
    let Some(profile) = shell_profile() else {
        return Ok(PathSetup::Manual);
    };
    let existing = std::fs::read_to_string(&profile).unwrap_or_default();
    let line = path_line(shim_dir);
    if existing.contains(PROFILE_MARKER) || existing.contains(&line) {
        // Written before but not active yet: the user has not started a new shell.
        return Ok(PathSetup::Added(profile));
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("\n{PROFILE_MARKER}\n{line}\n"));
    std::fs::write(&profile, out).with_context(|| format!("edit {}", profile.display()))?;
    Ok(PathSetup::Added(profile))
}

/// Install (or refresh) the shim. Returns (shim_path, shim_dir) so the caller
/// can print PATH guidance.
pub fn install(paths: &Paths) -> Result<(PathBuf, PathBuf)> {
    let shim = shim_path(paths);
    let shim_dir = shim
        .parent()
        .map(|p| p.to_path_buf())
        .context("shim path has no parent")?;
    let real = find_real_claude(&shim_dir)
        .context("could not find the real `claude` on PATH - install it first")?;
    let pointer = paths.store_dir().join("active-claude");
    // The shim calls back into THIS binary, by absolute path: whatever swapdex
    // installed the shim is the one that will start its proxy, even if PATH
    // later changes or a different build lands ahead of it.
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("swapdex"));
    std::fs::create_dir_all(&shim_dir).context("create shim dir")?;
    std::fs::write(&shim, shim_script(&pointer, &real, &me)).context("write shim")?;
    make_executable(&shim)?;
    Ok((shim, shim_dir))
}

/// Install the `codex` shim beside Claude's. Returns the path, or `None` when
/// there is no real `codex` on PATH to wrap - not having Codex installed is not
/// an error, it just means there is nothing to shim.
pub fn install_codex(paths: &Paths) -> Result<Option<PathBuf>> {
    let shim = shim_path_for(paths, "codex");
    let shim_dir = shim
        .parent()
        .map(|p| p.to_path_buf())
        .context("shim path has no parent")?;
    let Some(real) = find_real(&shim_dir, "codex") else {
        return Ok(None);
    };
    let pointer = paths.store_dir().join("active-codex");
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("swapdex"));
    std::fs::create_dir_all(&shim_dir).context("create shim dir")?;
    std::fs::write(&shim, codex_shim_script(&pointer, &real, &me)).context("write codex shim")?;
    make_executable(&shim)?;
    Ok(Some(shim))
}

fn make_executable(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
            .context("chmod shim")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // Codex reads CODEX_HOME the way Claude reads CLAUDE_CONFIG_DIR, so its shim
    // is the same shape: fill the home from the pointer, and only when nothing
    // has already chosen one - `swapdex run` sets it explicitly, and overriding
    // that would open every account as the default one.
    #[test]
    fn the_codex_shim_points_codex_home_at_the_default_slot() {
        let s = codex_shim_script(
            Path::new("/store/active-codex"),
            Path::new("/usr/bin/codex"),
            Path::new("/bin/swapdex"),
        );
        assert!(s.starts_with("#!/bin/sh"));
        assert!(
            s.contains("/store/active-codex"),
            "reads codex's own pointer"
        );
        assert!(s.contains("/usr/bin/codex"), "execs the real codex");
        assert!(s.contains("CODEX_HOME="), "sets the slot env");
        assert!(
            s.contains("if [ -z \"$CODEX_HOME\" ]"),
            "an explicit CODEX_HOME is a decision already made"
        );
        assert!(s.contains("exec "), "replaces the process");
        // It must never touch Claude's variables - one tool's shim moving the
        // other tool's account is the bug this whole split exists to prevent.
        assert!(!s.contains("CLAUDE_CONFIG_DIR"));
        assert!(!s.contains("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn script_references_pointer_real_claude_and_config_dir() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/bin/swapdex"),
        );
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("/store/active-claude"), "reads the pointer");
        assert!(s.contains("/usr/bin/claude"), "execs the real claude");
        assert!(s.contains("CLAUDE_CONFIG_DIR="), "sets the slot env");
        assert!(s.contains("exec "), "replaces the process");
    }

    // A running proxy is picked up automatically, and a STALE marker is not: the
    // pid gate is what keeps a killed proxy from sending claude at a dead port.
    // `swapdex run <account>` sets CLAUDE_CONFIG_DIR to that account's slot and
    // then execs claude - which finds the shim. If the shim overwrote it with the
    // default pointer (it did), every account opened as the default one, so
    // signing a second account in was impossible.
    #[test]
    fn an_explicit_config_dir_wins_over_the_default_pointer() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/bin/swapdex"),
        );
        assert!(
            s.contains("if [ -z \"$CLAUDE_CONFIG_DIR\" ]"),
            "the pointer only fills in when nothing chose a dir: {s}"
        );
        // The pointer is still applied when nothing else has.
        assert!(s.contains("/store/active-claude"), "{s}");
        assert!(s.contains("CLAUDE_CONFIG_DIR="), "{s}");
    }

    #[test]
    fn script_gets_its_proxy_from_swapdex_and_tolerates_none() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/bin/swapdex"),
        );
        assert!(
            s.contains("'/bin/swapdex' proxy --ensure"),
            "asks swapdex by absolute path, so the user starts nothing: {s}"
        );
        assert!(
            s.contains("ANTHROPIC_BASE_URL"),
            "points claude at the proxy"
        );
        assert!(
            s.contains("http://127.0.0.1:$port"),
            "loopback only, port from swapdex"
        );
        assert!(
            s.contains("2>/dev/null") && s.contains("if [ -n \"$port\" ]"),
            "no proxy is not an error - claude still runs: {s}"
        );
    }

    #[test]
    fn script_quotes_paths_with_spaces() {
        let s = shim_script(
            Path::new("/a b/active-claude"),
            Path::new("/c d/claude"),
            Path::new("/e f/swapdex"),
        );
        assert!(s.contains("'/a b/active-claude'"), "pointer is quoted");
        assert!(s.contains("'/e f/swapdex'"), "swapdex path is quoted");
        assert!(s.contains("'/c d/claude'"), "real claude is quoted");
    }

    #[test]
    fn recognizes_our_own_shim_by_marker() {
        // The generated shim carries the marker, so find_real_claude never bakes
        // a self-reference even if the shim dir is spelled oddly on PATH.
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("claude");
        std::fs::write(
            &shim,
            shim_script(Path::new("/p"), Path::new("/real"), Path::new("/sx")),
        )
        .unwrap();
        assert!(is_our_shim(&shim), "our shim is recognized by its marker");
        let real = dir.path().join("real-claude");
        std::fs::write(&real, "#!/bin/sh\nexec node /opt/claude \"$@\"\n").unwrap();
        assert!(!is_our_shim(&real), "a real claude is not flagged");
    }
}
