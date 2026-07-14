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
pub fn shim_script(pointer: &Path, real_claude: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # swapdex claude shim - launch claude in the default account's slot.\n\
         # Managed by swapdex; re-created by `swapdex shim`.\n\
         dir=$(cat {ptr} 2>/dev/null)\n\
         if [ -n \"$dir\" ]; then\n\
         \texec env CLAUDE_CONFIG_DIR=\"$dir\" {real} \"$@\"\n\
         else\n\
         \texec {real} \"$@\"\n\
         fi\n",
        ptr = sh_quote(pointer),
        real = sh_quote(real_claude),
    )
}

/// The first `claude` on PATH that is NOT swapdex's own shim dir - the real one
/// the shim should exec (avoids the shim calling itself).
fn find_real_claude(shim_dir: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir == shim_dir {
            continue;
        }
        let cand = dir.join("claude");
        if cand.is_file() {
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
    std::fs::create_dir_all(&shim_dir).context("create shim dir")?;
    std::fs::write(&shim, shim_script(&pointer, &real)).context("write shim")?;
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
        );
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("/store/active-claude"), "reads the pointer");
        assert!(s.contains("/usr/bin/claude"), "execs the real claude");
        assert!(s.contains("CLAUDE_CONFIG_DIR="), "sets the slot env");
        assert!(s.contains("exec "), "replaces the process");
    }

    #[test]
    fn script_quotes_paths_with_spaces() {
        let s = shim_script(Path::new("/a b/active-claude"), Path::new("/c d/claude"));
        assert!(s.contains("'/a b/active-claude'"), "pointer is quoted");
        assert!(s.contains("'/c d/claude'"), "real claude is quoted");
    }
}
