# swapdex Permanent Slots — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the no-copy slot foundation and the `swapdex run <name>` launcher — a Claude account launched in its own permanent `CLAUDE_CONFIG_DIR` slot, so its token rotates in place and never goes stale.

**Architecture:** A new `slots` registry (`src/slots.rs`) persists a name → slot mapping to `<store_dir>/slots.json`; each slot is a directory `<store_dir>/slots/<id>/`. `swapdex run <name>` resolves (or creates) the slot and `exec`s the tool with `CLAUDE_CONFIG_DIR` set to it — swapdex never writes the credential; the tool's own login does. This is additive: existing `add`/`use`/copy-switch and their tests are untouched.

**Tech Stack:** Rust, `clap` (CLI), `serde`/`serde_json` (registry), `sha2` (already a dependency; slot-id hashing). No new crates. Unix-only. No network.

## Global Constraints

- No network calls anywhere (swapdex is structurally offline). [spec §3]
- Never write a credential/Keychain item — the tool's own login creates it. [spec §4, §7]
- All paths go through `Paths`; every test runs under `SWAPDEX_ROOT` so no real login is touched. [paths.rs]
- Slot `config_dir` is always an absolute path under `store_dir()/slots/<id>`; the id is stable across renames (name-independent). [spec §6.1]
- Repo content (code, comments, docs, commit messages) in English; no emojis. [CLAUDE.md]
- Additive only in Phase 1: do not modify existing `add`, `use`, `login`, or the copy-switch path. [spec §10]
- Every task ends green: `cargo fmt --check`, `cargo clippy --all-targets` (no warnings), `cargo test`.

---

### Task 1: Slot registry (`src/slots.rs`)

**Files:**
- Create: `src/slots.rs`
- Modify: `src/lib.rs` (add `pub mod slots;`)

**Interfaces:**
- Consumes: `crate::paths::Paths` (`store_dir() -> PathBuf`).
- Produces:
  - `pub struct SlotRecord { pub name: String, pub id: String, pub config_dir: std::path::PathBuf, pub adopted: bool }`
  - `pub struct Slots { /* private */ }`
  - `Slots::open(paths: &Paths) -> anyhow::Result<Slots>`
  - `Slots::create(&mut self, name: &str) -> anyhow::Result<SlotRecord>` (creates the slot dir + persists; errors on duplicate/empty name)
  - `Slots::get(&self, name: &str) -> Option<SlotRecord>`
  - `Slots::list(&self) -> Vec<SlotRecord>`

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src/slots.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    #[test]
    fn create_persists_and_reloads() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let rec = {
            let mut s = Slots::open(&paths).unwrap();
            s.create("work").unwrap()
        };
        // config_dir is absolute and under the store's slots dir; the dir exists.
        assert!(rec.config_dir.is_absolute());
        assert!(rec.config_dir.starts_with(paths.store_dir().join("slots")));
        assert!(rec.config_dir.is_dir(), "slot dir was created");
        assert!(!rec.adopted);
        // A fresh open sees it (persisted to slots.json).
        let s2 = Slots::open(&paths).unwrap();
        assert_eq!(s2.get("work").unwrap().id, rec.id);
        assert_eq!(s2.list().len(), 1);
    }

    #[test]
    fn duplicate_and_empty_names_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        s.create("work").unwrap();
        assert!(s.create("work").is_err(), "duplicate name rejected");
        assert!(s.create("   ").is_err(), "empty name rejected");
    }

    #[test]
    fn id_is_stable_and_name_independent() {
        // Two slots created back-to-back get different ids (id is not the name).
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let a = s.create("alpha").unwrap();
        let b = s.create("beta").unwrap();
        assert_ne!(a.id, b.id);
        assert_ne!(a.id, "alpha", "id is opaque, not the display name");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib slots::`
Expected: FAIL to compile ("cannot find type `Slots`") — the module does not exist yet.

- [ ] **Step 3: Write minimal implementation**

Put this at the TOP of `src/slots.rs` (above the `#[cfg(test)] mod tests`):

```rust
//! The permanent-slot registry: a name -> slot mapping persisted to
//! `<store_dir>/slots.json`. Each slot is a directory under
//! `<store_dir>/slots/<id>/` used as a Claude `CLAUDE_CONFIG_DIR`. swapdex never
//! writes a credential into a slot; the tool's own login does. The id is opaque
//! and name-independent so a rename never changes the directory (and therefore
//! never changes the Keychain service, which is derived from the dir string).

use crate::paths::Paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotRecord {
    pub name: String,
    pub id: String,
    pub config_dir: PathBuf,
    #[serde(default)]
    pub adopted: bool,
}

pub struct Slots {
    file: PathBuf,
    slots_dir: PathBuf,
    records: Vec<SlotRecord>,
}

/// 16 hex chars of sha256(name + a monotonic-ish nanosecond stamp) — opaque and
/// stable once created. Not derived from the name alone, so a rename is free.
fn new_id(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(nanos.to_le_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Slots {
    pub fn open(paths: &Paths) -> Result<Slots> {
        let store = paths.store_dir();
        let file = store.join("slots.json");
        let records = if file.exists() {
            let bytes = std::fs::read(&file).context("read slots.json")?;
            serde_json::from_slice(&bytes).context("slots.json is corrupt")?
        } else {
            Vec::new()
        };
        Ok(Slots {
            file,
            slots_dir: store.join("slots"),
            records,
        })
    }

    pub fn get(&self, name: &str) -> Option<SlotRecord> {
        self.records.iter().find(|r| r.name == name).cloned()
    }

    pub fn list(&self) -> Vec<SlotRecord> {
        self.records.clone()
    }

    pub fn create(&mut self, name: &str) -> Result<SlotRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a slot name is required");
        }
        if self.records.iter().any(|r| r.name == name) {
            bail!("a slot named '{name}' already exists");
        }
        let id = new_id(name);
        let config_dir = self.slots_dir.join(&id);
        std::fs::create_dir_all(&config_dir).context("create slot dir")?;
        let rec = SlotRecord {
            name: name.to_string(),
            id,
            config_dir,
            adopted: false,
        };
        self.records.push(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).context("create store dir")?;
        }
        let bytes = serde_json::to_vec_pretty(&self.records)?;
        std::fs::write(&self.file, bytes).context("write slots.json")?;
        Ok(())
    }
}
```

Then register the module in `src/lib.rs` next to the other `pub mod` lines:

```rust
pub mod slots;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib slots::`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/slots.rs src/lib.rs
git commit -m "feat(slots): permanent-slot registry (name -> slot dir, persisted)"
```

---

### Task 2: `swapdex run <name>` launcher

**Files:**
- Modify: `src/main.rs` (add `Run` to the `Cmd` enum and its dispatch)
- Modify: `src/commands.rs` (add `pub fn run_account`)
- Test: `tests/run.rs` (new integration test file)

**Interfaces:**
- Consumes: `crate::slots::{Slots, SlotRecord}` from Task 1; `Paths`.
- Produces: `pub fn run_account(paths: &Paths, name: &str, args: &[String]) -> anyhow::Result<i32>` — resolves the slot (creating it if absent), then `exec`s `claude` with `CLAUDE_CONFIG_DIR` set to the slot dir. Only returns on failure to exec.

- [ ] **Step 1: Write the failing test**

Create `tests/run.rs`:

```rust
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

// A fake `claude` that prints the CLAUDE_CONFIG_DIR it was launched with, then
// prints any args. `swapdex run` exec's it, so its stdout is what we capture.
fn fake_claude(root: &Path) -> std::path::PathBuf {
    let dir = root.join("fakebin");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("claude");
    std::fs::write(
        &f,
        "#!/bin/sh\necho \"CFG=$CLAUDE_CONFIG_DIR\"\necho \"ARGS=$*\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

#[test]
fn run_launches_claude_in_the_accounts_slot() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin())
        .args(["run", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    // The slot dir was created under the store and passed as CLAUDE_CONFIG_DIR.
    let slots = root.path().join(".local/share/swapdex/slots");
    assert!(
        o.lines().any(|l| l.starts_with("CFG=") && l.contains(slots.to_str().unwrap())),
        "claude launched with the slot as CLAUDE_CONFIG_DIR: {o}"
    );
}

#[test]
fn run_forwards_extra_args_after_dash_dash() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin())
        .args(["run", "work", "--", "--resume", "abc"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(
        o.lines().any(|l| l.starts_with("ARGS=") && l.contains("--resume abc")),
        "extra args are forwarded to claude: {o}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test run`
Expected: FAIL — `swapdex run` is an unknown subcommand (clap error, non-zero exit; the assertions never see `CFG=`).

- [ ] **Step 3: Write minimal implementation**

In `src/commands.rs`, add (near the other `pub fn` command entry points):

```rust
/// Launch the tool in `<name>`'s permanent slot (create the slot on first use).
/// swapdex never writes the credential here - the tool's own login does, into
/// the slot's own `CLAUDE_CONFIG_DIR`. `exec` replaces this process, so this
/// only returns on failure.
pub fn run_account(paths: &Paths, name: &str, args: &[String]) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let mut slots = crate::slots::Slots::open(paths)?;
    let rec = match slots.get(name) {
        Some(r) => r,
        None => slots.create(name)?,
    };
    if !command_exists("claude") {
        eprintln!("swapdex: `claude` isn't on your PATH. Install it, then retry.");
        return Ok(3);
    }
    let err = std::process::Command::new("claude")
        .args(args)
        .env("CLAUDE_CONFIG_DIR", &rec.config_dir)
        .exec();
    Err(anyhow::anyhow!("failed to launch claude: {err}"))
}
```

In `src/main.rs`, add a variant to the `Cmd` enum (next to `Use`):

```rust
    /// Launch Claude in an account's own permanent slot (concurrent-safe)
    Run {
        /// The account to launch
        name: String,
        /// Extra args passed straight to `claude` (after `--`)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
```

And in the `match cmd` dispatch in `main.rs`, add:

```rust
        Cmd::Run { name, args } => commands::run_account(&paths, name, args),
```

Note: `command_exists` already exists in `commands.rs` (used by `login`). If `run_account` cannot see it, confirm it is in scope (same module).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test run`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/commands.rs tests/run.rs
git commit -m "feat: swapdex run <name> - launch claude in the account's slot"
```

---

### Task 3: `swapdex slots` — list the slots

**Files:**
- Modify: `src/main.rs` (add `Slots` to the `Cmd` enum and dispatch)
- Modify: `src/commands.rs` (add `pub fn list_slots`)
- Test: `tests/run.rs` (extend)

**Interfaces:**
- Consumes: `crate::slots::Slots` from Task 1.
- Produces: `pub fn list_slots(paths: &Paths) -> anyhow::Result<i32>` — prints one line per slot (`<name>  <config_dir>`), or a friendly empty-state line.

- [ ] **Step 1: Write the failing test**

Append to `tests/run.rs`:

```rust
#[test]
fn slots_lists_created_slots() {
    let root = tempfile::tempdir().unwrap();
    let bin_dir = fake_claude(root.path());
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // `run` creates the slot; then `slots` should list it.
    Command::new(bin())
        .args(["run", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let out = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(o.contains("work"), "the slot is listed: {o}");
}

#[test]
fn slots_empty_state_is_friendly() {
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["slots"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&out.stdout);
    assert!(o.to_lowercase().contains("no slots"), "empty-state hint: {o}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test run slots_`
Expected: FAIL — `swapdex slots` is an unknown subcommand.

- [ ] **Step 3: Write minimal implementation**

In `src/commands.rs`:

```rust
/// List the permanent slots (name + the config dir each launches into).
pub fn list_slots(paths: &Paths) -> Result<i32> {
    let slots = crate::slots::Slots::open(paths)?;
    let list = slots.list();
    if list.is_empty() {
        println!("No slots yet. Create one by launching an account: swapdex run <name>");
        return Ok(0);
    }
    for r in list {
        println!("  {}  {}", r.name, r.config_dir.display());
    }
    Ok(0)
}
```

In `src/main.rs`, add the enum variant:

```rust
    /// List the permanent account slots
    Slots,
```

And the dispatch arm:

```rust
        Cmd::Slots => commands::list_slots(&paths),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test run`
Expected: PASS (4 tests total in the file).

- [ ] **Step 5: Run the full gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/main.rs src/commands.rs tests/run.rs
git commit -m "feat: swapdex slots - list the permanent account slots"
```

---

## Self-Review

**1. Spec coverage (Phase 1 slice of spec §13):**
- Slot registry + creation → Task 1. ✓
- `swapdex run <name>` → Task 2. ✓
- Slot visibility (`ls`/`status` slot-aware, minimally) → Task 3 (`swapdex slots`). ✓ (Full `ls`/`status` unification is Phase 2, per spec §13.)
- No-copy invariant (swapdex never writes the credential) → Task 2 launches the tool's own login into the slot; swapdex writes nothing. ✓
- Path hygiene / stable id → Task 1 (`config_dir` absolute under store; id name-independent). ✓
- Additive / non-breaking → no existing command modified; new `Run`/`Slots` variants + new module only. ✓

**2. Placeholder scan:** No TBD/TODO; every code step shows complete code and exact commands.

**3. Type consistency:** `SlotRecord { name, id, config_dir, adopted }`, `Slots::{open,get,list,create}` used identically in Tasks 2 and 3 as defined in Task 1. `run_account(paths, name, args)` and `list_slots(paths)` signatures match their `main.rs` dispatch calls.

**Deferred to later phases (not gaps):** default-account pointer + `claude` shim + `use` repoint (Phase 2); adoption of existing `CLAUDE_CONFIG_DIR` dirs + migration + legacy retirement (Phase 3); shared-config symlinking and the `.claude.json` MCP question (spec §6.3, §14). The 0.25.0 running-session guard stays as the safety net until Phase 3.
