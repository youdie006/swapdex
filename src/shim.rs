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
    paths.store_dir().join("bin").join("claude")
}

/// Single-quote a path for safe embedding in the /bin/sh shim script.
fn sh_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shim script body. Reads the pointer; if a default is set, exec the real
/// claude with `CLAUDE_CONFIG_DIR`; otherwise exec the real claude unchanged.
///
/// It also picks up a RUNNING `swapdex proxy` and points claude at it, so
/// mid-session account switching works without the user exporting anything. The
/// marker carries the proxy's pid, and `kill -0` decides: a stale marker (the
/// proxy was killed) is ignored rather than sending claude at a dead port.
pub fn shim_script(pointer: &Path, real_claude: &Path, proxy_marker: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # swapdex claude shim - launch claude in the default account's slot.\n\
         # Managed by swapdex; re-created by `swapdex shim`.\n\
         p=$(cat {marker} 2>/dev/null)\n\
         if [ -n \"$p\" ]; then\n\
         \tpid=${{p%% *}}\n\
         \tport=${{p##* }}\n\
         \tif kill -0 \"$pid\" 2>/dev/null; then\n\
         \t\tANTHROPIC_BASE_URL=\"http://127.0.0.1:$port\"\n\
         \t\texport ANTHROPIC_BASE_URL\n\
         \tfi\n\
         fi\n\
         dir=$(cat {ptr} 2>/dev/null)\n\
         if [ -n \"$dir\" ]; then\n\
         \texec env CLAUDE_CONFIG_DIR=\"$dir\" {real} \"$@\"\n\
         else\n\
         \texec {real} \"$@\"\n\
         fi\n",
        marker = sh_quote(proxy_marker),
        ptr = sh_quote(pointer),
        real = sh_quote(real_claude),
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
    String::from_utf8_lossy(&buf[..n]).contains(SHIM_MARKER)
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
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir == shim_dir {
            continue;
        }
        let cand = dir.join("claude");
        if cand.is_file() && !is_our_shim(&cand) {
            return Some(cand);
        }
    }
    None
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
    let marker = proxy_marker(paths);
    std::fs::create_dir_all(&shim_dir).context("create shim dir")?;
    std::fs::write(&shim, shim_script(&pointer, &real, &marker)).context("write shim")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
            .context("chmod shim")?;
    }
    Ok((shim, shim_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn script_references_pointer_real_claude_and_config_dir() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/store/proxy"),
        );
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("/store/active-claude"), "reads the pointer");
        assert!(s.contains("/usr/bin/claude"), "execs the real claude");
        assert!(s.contains("CLAUDE_CONFIG_DIR="), "sets the slot env");
        assert!(s.contains("exec "), "replaces the process");
    }

    // A running proxy is picked up automatically, and a STALE marker is not: the
    // pid gate is what keeps a killed proxy from sending claude at a dead port.
    #[test]
    fn script_points_claude_at_a_live_proxy_only() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/store/proxy"),
        );
        assert!(s.contains("/store/proxy"), "reads the proxy marker");
        assert!(
            s.contains("ANTHROPIC_BASE_URL"),
            "points claude at the proxy"
        );
        assert!(s.contains("kill -0"), "ignores a stale marker");
        assert!(
            s.contains("http://127.0.0.1:$port"),
            "loopback only, port from the marker"
        );
    }

    #[test]
    fn script_quotes_paths_with_spaces() {
        let s = shim_script(
            Path::new("/a b/active-claude"),
            Path::new("/c d/claude"),
            Path::new("/e f/proxy"),
        );
        assert!(s.contains("'/a b/active-claude'"), "pointer is quoted");
        assert!(s.contains("'/e f/proxy'"), "marker is quoted");
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
            shim_script(Path::new("/p"), Path::new("/real"), Path::new("/m")),
        )
        .unwrap();
        assert!(is_our_shim(&shim), "our shim is recognized by its marker");
        let real = dir.path().join("real-claude");
        std::fs::write(&real, "#!/bin/sh\nexec node /opt/claude \"$@\"\n").unwrap();
        assert!(!is_our_shim(&real), "a real claude is not flagged");
    }
}
