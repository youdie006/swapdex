//! All credential writes go through here: 0600 at creation, temp in the dest's
//! own directory, atomic same-fs rename, symlink/ownership refusal. A write that
//! cannot be done atomically fails loudly rather than falling back to a
//! non-atomic (and mode-racy) copy.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Refuse to mutate real credentials as root: euid 0 on a shared box is a
/// footgun (and can write through another user's files).
pub fn ensure_not_root() -> Result<()> {
    if unsafe { libc_geteuid() } == 0 {
        bail!("refusing to run as root (uid 0) for credential operations");
    }
    Ok(())
}

/// Refuse a symlink, and (if the file exists) a file not owned by the current
/// user - a planted link or a foreign-owned file could redirect a read/write.
fn refuse_unsafe_path(path: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            bail!("refusing to operate on a symlink: {}", path.display());
        }
        if meta.uid() != unsafe { libc_geteuid() } {
            bail!(
                "refusing: {} is not owned by the current user",
                path.display()
            );
        }
    }
    Ok(())
}

/// Refuse a world-writable parent dir (unless sticky) - anyone could swap files
/// under it.
fn refuse_insecure_parent(dir: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(dir) {
        let mode = meta.mode();
        if mode & 0o002 != 0 && mode & 0o1000 == 0 {
            bail!("refusing: parent dir {} is world-writable", dir.display());
        }
    }
    Ok(())
}

/// Refuse if any directory component of `dest` at or below `root` is a
/// symlink, and require `dest` to actually live under `root`. The store
/// subtree is entirely swapdex-managed and never legitimately contains a
/// symlink, so a symlinked `accounts/<name>` or a symlinked store dir - which
/// the leaf-only check misses - can no longer redirect a token write outside
/// the 0700 store. (Ancestors ABOVE `root` are the user's own - e.g. a
/// dotfile-managed ~/.local - and are not policed here.)
pub fn refuse_symlink_below(root: &Path, dest: &Path) -> Result<()> {
    // The root itself must be a real directory, not a symlink to elsewhere.
    if let Ok(meta) = std::fs::symlink_metadata(root) {
        if meta.file_type().is_symlink() {
            bail!("refusing: store path {} is a symlink", root.display());
        }
    }
    let rel = dest.strip_prefix(root).map_err(|_| {
        anyhow::anyhow!(
            "refusing: {} is not under the store {}",
            dest.display(),
            root.display()
        )
    })?;
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                bail!("refusing: {} is a symlink inside the store", cur.display());
            }
        }
    }
    Ok(())
}

/// Read a whole file, refusing a symlink or a foreign-owned file.
pub fn read_regular(path: &Path) -> Result<Vec<u8>> {
    refuse_unsafe_path(path)?;
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

/// Write bytes to `dest` atomically at mode 0600. The temp file is created in
/// the destination's OWN directory (so rename is same-filesystem) with mode
/// 0600 at creation (no create-then-chmod world-readable window).
pub fn write_secret(dest: &Path, bytes: &[u8]) -> Result<()> {
    refuse_unsafe_path(dest)?;
    let dir = dest.parent().context("destination has no parent dir")?;
    if !dir.exists() {
        // A parent we create ourselves (e.g. a fresh ~/.codex) will hold
        // credential files - 0700 it, not the umask-default 0755.
        std::fs::create_dir_all(dir).ok();
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    refuse_insecure_parent(dir)?;
    let tmp = dir.join(format!(
        ".{}.swapdex.tmp",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("cred")
    ));
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create temp {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // rename replaces the destination NAME atomically and does not follow a
    // symlink at the destination, so the write is symlink-safe by construction.
    std::fs::rename(&tmp, dest)
        .with_context(|| format!("atomic rename {} -> {}", tmp.display(), dest.display()))?;
    // fsync the directory so the rename itself is durable across a crash.
    if let Ok(dirf) = std::fs::File::open(dir) {
        let _ = dirf.sync_all();
    }
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600)).ok();
    Ok(())
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn write_secret_is_0600_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".credentials.json");
        write_secret(&dest, b"{\"token\":\"x\"}").unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credential file must be 0600");
        assert_eq!(std::fs::read(&dest).unwrap(), b"{\"token\":\"x\"}");
    }

    #[test]
    fn write_secret_refuses_a_symlinked_destination() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"other").unwrap();
        let dest = dir.path().join("link");
        symlink(&outside, &dest).unwrap();
        assert!(write_secret(&dest, b"secret").is_err());
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"other",
            "symlink target untouched"
        );
    }

    #[test]
    fn read_regular_refuses_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.path().join("l");
        symlink(&target, &link).unwrap();
        assert!(read_regular(&link).is_err());
    }
}
