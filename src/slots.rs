//! The permanent-slot registry: a name -> slot mapping persisted to
//! `<store_dir>/slots.json`. Each slot is a directory under
//! `<store_dir>/slots/<id>/` handed to the tool as its own home - Claude's
//! `CLAUDE_CONFIG_DIR`, Codex's `CODEX_HOME`. swapdex never writes a credential
//! into a slot; the tool's own login does. The id is opaque and
//! name-independent so a rename never changes the directory (and therefore never
//! changes the Keychain service, which is derived from the dir string).
//!
//! A registry is opened FOR one tool and sees only that tool's slots. The same
//! account name on two tools is two accounts, and each tool has its own default
//! pointer, so switching Codex never moves where Claude launches.

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
    /// Which tool this slot is a home for. Absent in registries written before
    /// tools were distinguished, and those are Claude's - the only kind that
    /// existed - so the default keeps an upgraded install working untouched.
    #[serde(default = "default_tool")]
    pub tool: String,
}

fn default_tool() -> String {
    "claude-code".to_string()
}

pub struct Slots {
    file: PathBuf,
    slots_dir: PathBuf,
    /// Every slot on disk, of every tool: writes must preserve the tools this
    /// registry is not scoped to rather than dropping them.
    all: Vec<SlotRecord>,
    /// Just this tool's, in registry order.
    records: Vec<SlotRecord>,
    tool: String,
}

/// The environment variable a tool reads to find the home it should use.
pub fn home_var(tool: &str) -> Option<&'static str> {
    match tool {
        "claude-code" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        _ => None,
    }
}

/// Names that read as a tool's own home rather than as an account.
///
/// A slot called `claude` looks like it points at `~/.claude`. It does not - it
/// points wherever that slot was made, which for a migrated profile is a fresh
/// directory inside swapdex's own store. That misreading cost a real user their
/// bearings: they went looking for conversations in the account named `claude`
/// and the ones they wanted were in `~/.claude`, a different account entirely.
pub fn name_reads_as_a_tool_home(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(
        n.as_str(),
        "claude" | "claude-code" | "codex" | "gemini" | "antigravity" | ".claude" | ".codex"
    )
}

/// Slots that turn out to hold the SAME account, grouped.
///
/// Two directories can hold one login, and nothing on screen said so: the fleet
/// read as four accounts when three were distinct, and a rate limit hit on one
/// applies to its twin. Worth stating plainly - keeping two directories for one
/// account is a fair thing to do, but only if you know that is what you have.
///
/// A slot whose identity cannot be read is never grouped. Two unknowns are not
/// evidence of one account.
pub fn slots_sharing_an_account(named: &[(String, Option<String>)]) -> Vec<Vec<String>> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for (name, uuid) in named {
        let Some(u) = uuid else { continue };
        match groups.iter_mut().find(|(k, _)| k == u) {
            Some((_, names)) => names.push(name.clone()),
            None => groups.push((u.clone(), vec![name.clone()])),
        }
    }
    groups
        .into_iter()
        .map(|(_, names)| names)
        .filter(|names| names.len() > 1)
        .collect()
}

/// A name that will not be mistaken for a tool's home, built from one that is.
pub fn suggest_non_colliding(name: &str, taken: &[String]) -> String {
    let base = format!("{}-account", name.trim().to_ascii_lowercase());
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}{i}"))
        .find(|c| !taken.iter().any(|t| t == c))
        .unwrap_or(base)
}

/// 16 hex chars of sha256(name + a monotonic-ish nanosecond stamp) - opaque and
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

/// Refuse a name that reads as a tool's own home, naming one that does not.
fn reject_tool_home_name(name: &str) -> Result<()> {
    if name_reads_as_a_tool_home(name) {
        bail!(
            "'{name}' reads as the tool's own home directory, not as an account - \
             a slot by that name points wherever it was made, which is somewhere else \
             entirely. Pick something that names the ACCOUNT (its owner, its purpose): \
             e.g. '{}'",
            suggest_non_colliding(name, &[])
        );
    }
    Ok(())
}

impl Slots {
    /// Claude's registry - the established caller, kept so every existing call
    /// site keeps meaning what it meant.
    pub fn open(paths: &Paths) -> Result<Slots> {
        Self::open_for(paths, "claude-code")
    }

    /// The registry as `tool` sees it.
    pub fn open_for(paths: &Paths, tool: &str) -> Result<Slots> {
        let store = paths.store_dir();
        let file = store.join("slots.json");
        let all: Vec<SlotRecord> = if file.exists() {
            let bytes = std::fs::read(&file).context("read slots.json")?;
            serde_json::from_slice(&bytes).context("slots.json is corrupt")?
        } else {
            Vec::new()
        };
        let records = all.iter().filter(|r| r.tool == tool).cloned().collect();
        Ok(Slots {
            file,
            slots_dir: store.join("slots"),
            all,
            records,
            tool: tool.to_string(),
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
        reject_tool_home_name(name)?;
        let id = new_id(name);
        let config_dir = self.slots_dir.join(&id);
        std::fs::create_dir_all(&config_dir).context("create slot dir")?;
        let rec = SlotRecord {
            name: name.to_string(),
            id,
            config_dir,
            adopted: false,
            tool: self.tool.clone(),
        };
        self.records.push(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    /// Rename a slot. Only the NAME changes: the id and directory stay, which is
    /// what keeps the macOS Keychain item (derived from the directory string)
    /// valid - renaming must never cost an account its login.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<bool> {
        let new = new.trim();
        if new.is_empty() {
            bail!("a slot name is required");
        }
        if self.records.iter().any(|r| r.name == new) {
            bail!("an account named '{new}' already exists");
        }
        // Renaming INTO the confusing name is the same mistake as creating it.
        reject_tool_home_name(new)?;
        let Some(r) = self.records.iter_mut().find(|r| r.name == old) else {
            return Ok(false);
        };
        r.name = new.to_string();
        self.persist()?;
        Ok(true)
    }

    /// Unregister a slot: drop the name -> directory mapping. The DIRECTORY is
    /// left alone, always. It holds that account's login, and for an adopted dir
    /// it is somewhere the user chose - deleting either would turn "stop managing
    /// this" into "lose this account". Re-registering restores it.
    /// Returns false when there was no such slot.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let Some(i) = self.records.iter().position(|r| r.name == name) else {
            return Ok(false);
        };
        let gone = self.records.remove(i);
        // A dangling default pointer would send a plain `claude` at a directory
        // swapdex no longer knows about; clear it when it named this slot.
        if self.default_dir().as_deref() == Some(gone.config_dir.as_path()) {
            let _ = std::fs::remove_file(self.pointer_file());
        }
        // Same for the serving pointer, and read from the file rather than
        // through serving_dir(): the record is already out of the list, so the
        // accessor answers None here by design. Leaving the file would have a
        // later `adopt` of the same directory silently resume paying.
        let served = std::fs::read_to_string(self.serving_file()).unwrap_or_default();
        if served.trim() == gone.config_dir.to_string_lossy() {
            let _ = std::fs::remove_file(self.serving_file());
        }
        self.persist()?;
        Ok(true)
    }

    fn persist(&mut self) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).context("create store dir")?;
        }
        // One file holds every tool's slots. Writing only the scoped view would
        // delete the other tools' accounts, so the others are carried through
        // untouched and this tool's are replaced wholesale.
        let mut out: Vec<SlotRecord> = self
            .all
            .iter()
            .filter(|r| r.tool != self.tool)
            .cloned()
            .collect();
        out.extend(self.records.iter().cloned());
        let bytes = serde_json::to_vec_pretty(&out)?;
        std::fs::write(&self.file, bytes).context("write slots.json")?;
        self.all = out;
        Ok(())
    }

    /// Register an EXISTING config dir as a slot without creating or moving it
    /// (adoption of a `~/.claude-*` the user already uses). `config_dir` must be
    /// an existing absolute path.
    pub fn adopt(&mut self, name: &str, config_dir: &std::path::Path) -> Result<SlotRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a slot name is required");
        }
        if self.records.iter().any(|r| r.name == name) {
            bail!("a slot named '{name}' already exists");
        }
        reject_tool_home_name(name)?;
        if !config_dir.is_absolute() {
            bail!("config dir must be an absolute path");
        }
        if !config_dir.is_dir() {
            bail!("config dir does not exist: {}", config_dir.display());
        }
        // Before this directory is registered, drop a serving pointer that names
        // nothing. Registering is what would turn such a pointer from inert back
        // into a live instruction to pay, and nobody asked for that.
        self.prune_serving();
        let rec = SlotRecord {
            name: name.to_string(),
            id: new_id(name),
            config_dir: config_dir.to_path_buf(),
            adopted: true,
            tool: self.tool.clone(),
        };
        self.records.push(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    /// The pointer file this tool's shim reads to find the default account's
    /// slot: `<store_dir>/active-claude`, `<store_dir>/active-codex`. Claude's
    /// keeps the name it has always had, so upgrading never loses a default.
    fn pointer_file(&self) -> PathBuf {
        // `self.file` is `<store_dir>/slots.json`; its parent is the store dir.
        let store = self
            .file
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let short = match self.tool.as_str() {
            "claude-code" => "claude",
            other => other,
        };
        store.join(format!("active-{short}"))
    }

    /// The pointer naming the account that SERVES turns, when that is not simply
    /// the account you launched in: `<store_dir>/serving-claude`.
    fn serving_file(&self) -> PathBuf {
        let mut p = self.pointer_file();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().replace("active-", "serving-"))
            .unwrap_or_else(|| "serving-claude".into());
        p.set_file_name(name);
        p
    }

    /// Hand turns to `name` WITHOUT moving where new sessions start.
    ///
    /// These are two different questions - where a conversation lives, and who
    /// pays for its turns - and answering both with one pointer is what made a
    /// user's conversations appear to vanish when they only meant to change who
    /// was paying.
    pub fn set_serving(&self, name: &str) -> Result<()> {
        let rec = self
            .get(name)
            .with_context(|| format!("no account named '{name}'"))?;
        let p = self.serving_file();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).context("create store dir")?;
        }
        std::fs::write(&p, rec.config_dir.to_string_lossy().as_bytes())
            .with_context(|| format!("write {} serving pointer", self.tool))
    }

    /// Delete a serving pointer that names no registered account.
    ///
    /// `serving_dir` already refuses to answer with one, so this is not about
    /// the answer - it is about the trap. A path outlives the account that owned
    /// it, and registering that same directory again would make a dead pointer
    /// live, silently paying for turns nobody assigned to it.
    pub fn prune_serving(&self) {
        let Ok(s) = std::fs::read_to_string(self.serving_file()) else {
            return;
        };
        let dir = PathBuf::from(s.trim());
        if dir.as_os_str().is_empty() {
            return;
        }
        if !self.list().iter().any(|r| r.config_dir == dir) {
            let _ = std::fs::remove_file(self.serving_file());
        }
    }

    /// Stop directing turns anywhere in particular: the account a session was
    /// launched in pays for it, which is what anyone would assume by default.
    pub fn clear_serving(&self) -> Result<()> {
        match std::fs::remove_file(self.serving_file()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("clear serving pointer"),
        }
    }

    /// The slot dir of the account serving turns, if one was named AND that
    /// account is still registered.
    ///
    /// The pointer holds a path, and a path outlives the account that owned it.
    /// Removing the serving account left the file behind, so turns stayed
    /// nominally directed at a home no account owns - and the one this was found
    /// on had no login in it. An answer here is a claim about who pays, so it is
    /// only given for an account that still exists.
    /// The file recording which account was ASKED to serve. Its timestamp is
    /// how a caller tells whether the proxy has had its say since.
    pub fn serving_pointer_file(&self) -> PathBuf {
        self.serving_file()
    }

    pub fn serving_dir(&self) -> Option<PathBuf> {
        let s = std::fs::read_to_string(self.serving_file()).ok()?;
        let dir = PathBuf::from(s.trim());
        if dir.as_os_str().is_empty() {
            return None;
        }
        self.list()
            .into_iter()
            .any(|r| r.config_dir == dir)
            .then_some(dir)
    }

    /// The account that pays the next turn through the proxy: the one directing
    /// turns, or the default it falls back to when none does. This is the same
    /// resolution the proxy performs, kept in one place so what a screen claims
    /// and what the proxy does cannot drift apart.
    pub fn payer(&self) -> Option<String> {
        let dir = self.serving_dir().or_else(|| self.default_dir())?;
        self.list()
            .into_iter()
            .find(|r| r.config_dir == dir)
            .map(|r| r.name)
    }

    /// Point the default account at `name`'s slot. A plain `claude` (via the
    /// shim) then launches in this slot. No credential is moved.
    pub fn set_default(&self, name: &str) -> Result<()> {
        let rec = self
            .get(name)
            .with_context(|| format!("no slot named '{name}'"))?;
        let p = self.pointer_file();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).context("create store dir")?;
        }
        std::fs::write(&p, rec.config_dir.to_string_lossy().as_bytes())
            .with_context(|| format!("write {} pointer", self.tool))?;
        // Starting somewhere new settles who pays for it too, so the two answers
        // cannot drift into a combination nobody asked for.
        self.clear_serving()?;
        Ok(())
    }

    /// The default account's slot dir, if a default has been set.
    pub fn default_dir(&self) -> Option<PathBuf> {
        let s = std::fs::read_to_string(self.pointer_file()).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| PathBuf::from(s))
    }
}

/// The account-agnostic things symlinked from the bare home dir into a
/// freshly-created slot, so switching accounts changes who PAYS and nothing
/// else. The token and the file holding account identity stay per-slot and are
/// never linked. (For Claude, MCP config lives inside `.claude.json`, which is
/// per-account; sharing it needs the resolution noted in the design's open
/// questions, so it is intentionally left per-slot.)
///
/// `projects/` - the conversations - is shared, and that is the point of the
/// whole tool. A transcript carries no account or organisation identifier
/// anywhere in it, so it belongs to the person, not to whichever account
/// happened to pay for those turns. Kept per-slot, `swapdex use B` made every
/// conversation started on A vanish from `claude --resume`: not lost, but
/// invisible, which for a resume list is the same thing.
pub const SHARED_CONFIG_FILES: &[&str] = &["settings.json", "CLAUDE.md", "projects"];

/// Codex keeps its settings in `config.toml` and its project instructions in
/// `AGENTS.md`; its credential lives apart in `auth.json`, which is what makes
/// the same split work. `sessions/` is shared for the same reason Claude's
/// `projects/` is - a conversation is not the property of the account that
/// funded it. Note swapdex also READS rate limits out of there, and sharing
/// means a reading found in one slot describes whichever account wrote it;
/// `codex_usage` asks the account directly and no longer depends on that.
pub const SHARED_CONFIG_FILES_CODEX: &[&str] = &["config.toml", "AGENTS.md", "sessions"];

/// Do this tool's accounts all read one conversation history?
///
/// True when every slot's history directory is a link rather than a directory
/// of its own. Used to decide whether a switch still needs the old warning
/// about conversations going out of view - a warning that is right for an
/// un-repaired install and wrong, and misleading, for a repaired one.
pub fn history_is_shared(paths: &crate::paths::Paths, tool: &str) -> bool {
    let dir_name = if tool == "codex" {
        "sessions"
    } else {
        "projects"
    };
    let Ok(slots) = Slots::open_for(paths, tool) else {
        return true; // nothing registered: nothing to warn about
    };
    slots.list().into_iter().all(|r| {
        let own = r.config_dir.join(dir_name);
        // A real directory of its own is the un-shared case. A link, or nothing
        // at all, is not.
        !std::fs::symlink_metadata(&own).is_ok_and(|m| m.file_type().is_dir())
    })
}

/// Copy the conversations a slot holds alone into the shared store.
///
/// Slots made before sharing have their own `projects/`, and those
/// conversations exist nowhere else. Before a slot can point at the shared
/// store they have to be carried over, or switching accounts would hide them -
/// which is the very thing sharing is meant to end.
///
/// COPIES, never moves: a half-finished merge must leave every original
/// readable. And an entry already in the shared store is never overwritten -
/// that one is what every account can see, and a slot's copy of the same
/// conversation is at best equally good and at worst older.
///
/// `dry_run` counts what WOULD be carried and writes nothing. A preview that
/// changes the thing it is previewing is worse than no preview: the answer it
/// gives is only true the first time, and the second run reports zero because
/// the first one already did the work.
///
/// Returns how many were carried.
pub fn carry_history_into_shared(
    slot_dir: &std::path::Path,
    shared_dir: &std::path::Path,
    dry_run: bool,
) -> std::io::Result<usize> {
    if !slot_dir.is_dir() {
        return Ok(0);
    }
    let mut carried = 0;
    let mut stack = vec![slot_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(slot_dir) else {
                continue;
            };
            let dest = shared_dir.join(rel);
            if dest.exists() {
                continue;
            }
            if !dry_run {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&path, &dest)?;
            }
            carried += 1;
        }
    }
    Ok(carried)
}

/// Find a slot by name across EVERY tool, and say which tool it belongs to.
///
/// Callers that act on "an account" by name - rename, remove - must not assume
/// Claude. Looking only there meant a Codex slot was never found: its snapshot
/// was renamed and the slot kept its old name, so one account answered to two.
pub fn find_any_tool(paths: &crate::paths::Paths, name: &str) -> Option<(String, SlotRecord)> {
    for tool in ["claude-code", "codex", "gemini", "antigravity"] {
        if let Ok(s) = Slots::open_for(paths, tool) {
            if let Some(r) = s.get(name) {
                return Some((tool.to_string(), r));
            }
        }
    }
    None
}

/// The files `tool` shares across its accounts.
pub fn shared_files(tool: &str) -> &'static [&'static str] {
    match tool {
        "codex" => SHARED_CONFIG_FILES_CODEX,
        _ => SHARED_CONFIG_FILES,
    }
}

/// Symlink the shared config files from `source` into `slot` (best-effort; skips
/// files absent in source or already present in the slot). Returns the names
/// linked.
pub fn link_shared_config(
    slot: &std::path::Path,
    source: &std::path::Path,
    tool: &str,
) -> Vec<String> {
    let mut linked = Vec::new();
    for name in shared_files(tool) {
        let src = source.join(name);
        let dst = slot.join(name);
        if src.exists() && !dst.exists() {
            #[cfg(unix)]
            if std::os::unix::fs::symlink(&src, &dst).is_ok() {
                linked.push((*name).to_string());
            }
        }
    }
    linked
}

#[cfg(test)]
mod shared_history_tests {
    use super::*;

    /// A conversation is not the property of the account that paid for it. The
    /// transcripts carry no account or organisation identifier at all - checked
    /// on a real machine, seven keys, zero hits - and the whole point of running
    /// the proxy is that the account is a payment method, not a filing cabinet.
    ///
    /// Left per-slot, `swapdex use B` made every conversation started on A
    /// vanish from `claude --resume`. They were not lost, but they were
    /// invisible, which for a resume list is the same thing.
    #[test]
    fn a_conversation_is_reachable_from_every_account() {
        assert!(
            shared_files("claude-code").contains(&"projects"),
            "Claude's transcripts must be shared across slots"
        );
        assert!(
            shared_files("codex").contains(&"sessions"),
            "Codex's transcripts must be shared across slots"
        );
    }

    /// What must NOT be shared: the credential, and the file naming who the
    /// account is. Sharing either is how one account starts answering as
    /// another.
    #[test]
    fn identity_and_credentials_stay_with_their_account() {
        for tool in ["claude-code", "codex"] {
            for private in [".credentials.json", ".claude.json", "auth.json"] {
                assert!(
                    !shared_files(tool).contains(&private),
                    "{tool} must not share {private}"
                );
            }
        }
    }
}

#[cfg(test)]
mod sharing_tests {
    use super::*;

    fn n(v: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.map(str::to_string)))
            .collect()
    }

    /// The real shape of 병승's Mac, found only by reading uuids by hand:
    /// ~/.claude and ~/.claude-company are one login in two directories.
    #[test]
    fn two_directories_holding_one_login_are_reported_together() {
        let got = slots_sharing_an_account(&n(&[
            ("bsgong", Some("8dd1a9aa")),
            ("rnd", Some("202743db")),
            ("bsgong-slot", Some("8dd1a9aa")),
        ]));
        assert_eq!(
            got,
            vec![vec!["bsgong".to_string(), "bsgong-slot".to_string()]]
        );
    }

    /// Two identities nobody can read are not evidence of one account.
    #[test]
    fn unreadable_identities_are_never_grouped() {
        assert!(slots_sharing_an_account(&n(&[("a", None), ("b", None)])).is_empty());
    }

    #[test]
    fn distinct_accounts_say_nothing() {
        assert!(slots_sharing_an_account(&n(&[("a", Some("u1")), ("b", Some("u2"))])).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    /// A serving pointer outlives the account it named: remove that account and
    /// the file still holds its directory. Answering with it means turns are
    /// nominally directed at a home nobody owns - on the machine this was found,
    /// at one with no login in it at all. Whoever is paying must be an account
    /// that still exists, or nobody.
    #[test]
    fn a_serving_pointer_to_a_removed_account_names_nobody() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut s = Slots::open_for(&paths, "codex").unwrap();
            s.create("company").unwrap();
            s.set_serving("company").unwrap();
        }
        assert!(
            Slots::open_for(&paths, "codex")
                .unwrap()
                .serving_dir()
                .is_some(),
            "the pointer answers while the account is there"
        );
        {
            let mut s = Slots::open_for(&paths, "codex").unwrap();
            s.remove("company").unwrap();
        }
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().serving_dir(),
            None,
            "and stops the moment the account it named is gone"
        );
    }

    /// The removal also takes the file with it, so re-adopting the same
    /// directory later does not silently resume paying for turns.
    #[test]
    fn removing_the_serving_account_takes_the_pointer_too() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let dir = {
            let mut s = Slots::open_for(&paths, "codex").unwrap();
            let rec = s.create("company").unwrap();
            s.set_serving("company").unwrap();
            s.remove("company").unwrap();
            rec.config_dir
        };
        let mut s = Slots::open_for(&paths, "codex").unwrap();
        s.adopt("company", &dir).unwrap();
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().serving_dir(),
            None,
            "adopting the same directory back does not make it serve again"
        );
    }

    /// What the shim writes on Codex's status screen has to be the account the
    /// proxy will actually bill, which is the one directing turns OR - far more
    /// often, because nobody has run `serve` - the default it falls back to.
    /// Naming only the explicit case leaves the common case anonymous, which is
    /// the complaint that started this.
    #[test]
    fn the_payer_is_the_server_or_the_default_behind_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut s = Slots::open_for(&paths, "codex").unwrap();
            s.create("main").unwrap();
            s.create("company").unwrap();
            s.set_default("main").unwrap();
        }
        let open = || Slots::open_for(&paths, "codex").unwrap();
        assert_eq!(
            open().payer().as_deref(),
            Some("main"),
            "with nobody serving, the default pays"
        );
        open().set_serving("company").unwrap();
        assert_eq!(
            open().payer().as_deref(),
            Some("company"),
            "and the account directing turns takes over"
        );
        open().remove("company").unwrap();
        assert_eq!(
            open().payer().as_deref(),
            Some("main"),
            "and it hands back when that account is gone"
        );
    }

    /// A pointer written before the account was removed - or before removal
    /// learned to take it along - sits on disk naming a directory nobody holds.
    /// serving_dir() refuses to answer with it, so it is inert; adopt that same
    /// directory back and it is a live pointer again, silently paying.
    #[test]
    fn adopting_a_directory_a_dead_pointer_names_does_not_resume_paying() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let orphan = root.path().join("codex-company");
        std::fs::create_dir_all(&orphan).unwrap();
        let s = Slots::open_for(&paths, "codex").unwrap();
        std::fs::create_dir_all(paths.store_dir()).unwrap();
        std::fs::write(s.serving_file(), orphan.to_string_lossy().as_bytes()).unwrap();
        drop(s);

        let mut s = Slots::open_for(&paths, "codex").unwrap();
        s.adopt("company", &orphan).unwrap();
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().serving_dir(),
            None,
            "the directory is registered again, but nobody asked it to pay"
        );
    }

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

    // Codex isolates an account the same way Claude does - its own home dir via
    // CODEX_HOME - so the slot model has to hold both without either tool seeing
    // the other's accounts or moving the other's pointer.
    #[test]
    fn slots_are_scoped_to_their_tool() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut c = Slots::open_for(&paths, "claude-code").unwrap();
            c.create("work").unwrap();
        }
        {
            // The SAME name on another tool is a different account, not a clash.
            let mut x = Slots::open_for(&paths, "codex").unwrap();
            x.create("work").unwrap();
        }
        let c = Slots::open_for(&paths, "claude-code").unwrap();
        let x = Slots::open_for(&paths, "codex").unwrap();
        assert_eq!(c.list().len(), 1, "claude sees only its own");
        assert_eq!(x.list().len(), 1, "codex sees only its own");
        assert_ne!(
            c.get("work").unwrap().config_dir,
            x.get("work").unwrap().config_dir,
            "two tools never share a directory"
        );
        // Pointers are independent: switching Codex must not move Claude.
        c.set_default("work").unwrap();
        assert_eq!(c.default_dir(), Some(c.get("work").unwrap().config_dir));
        assert_eq!(x.default_dir(), None, "codex has no default yet");
        x.set_default("work").unwrap();
        assert_eq!(c.default_dir(), Some(c.get("work").unwrap().config_dir));
        assert_eq!(x.default_dir(), Some(x.get("work").unwrap().config_dir));
    }

    // Slots registered before tools were distinguished are Claude's - that is the
    // only kind that existed - and must keep working across the upgrade.
    #[test]
    fn a_slot_recorded_without_a_tool_is_claudes() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        std::fs::create_dir_all(paths.store_dir()).unwrap();
        std::fs::write(
            paths.store_dir().join("slots.json"),
            br#"[{"name":"old","id":"abc","config_dir":"/tmp/old-slot","adopted":true}]"#,
        )
        .unwrap();
        let c = Slots::open_for(&paths, "claude-code").unwrap();
        assert_eq!(c.list().len(), 1, "the pre-upgrade slot still lists");
        assert_eq!(c.get("old").unwrap().tool, "claude-code");
        let x = Slots::open_for(&paths, "codex").unwrap();
        assert!(x.list().is_empty(), "it was never a codex slot");
        // And Claude's pointer file keeps its established name, so an upgrade
        // does not silently unset the user's default account.
        c.set_default("old").unwrap();
        assert!(paths.store_dir().join("active-claude").exists());
    }

    // A slot called `claude` reads as ~/.claude and is not: it points wherever it
    // was made. A real user lost their bearings on exactly that, so the name is
    // refused at the point it would be created rather than explained afterwards.
    // Two different questions were answered by one pointer: WHERE a new session
    // starts (which decides the conversations `-r` can offer) and WHO pays for a
    // turn. A user who only wanted the second got the first as well, and their
    // conversations appeared to vanish.
    #[test]
    fn where_you_start_and_who_serves_are_separate_answers() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let home = s.create("home").unwrap();
        let payer = s.create("payer").unwrap();

        s.set_default("home").unwrap();
        assert_eq!(s.default_dir(), Some(home.config_dir.clone()));
        // Nothing has been said about who serves, so the account you launch in
        // pays for its own turns - the behaviour anyone would assume.
        assert_eq!(s.serving_dir(), None);

        // Hand the turns to another account WITHOUT moving where sessions start.
        s.set_serving("payer").unwrap();
        assert_eq!(s.serving_dir(), Some(payer.config_dir.clone()));
        assert_eq!(
            s.default_dir(),
            Some(home.config_dir.clone()),
            "the conversation store is untouched - this is the whole point"
        );

        // Moving where you start also settles who pays, so the two cannot drift
        // into a state nobody asked for.
        s.set_default("payer").unwrap();
        assert_eq!(s.serving_dir(), None, "a fresh start pays for itself");
    }

    #[test]
    fn a_name_that_reads_as_a_tools_home_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        for bad in ["claude", "Claude", "codex", "claude-code", ".claude"] {
            let e = s.create(bad).expect_err("refused");
            let msg = e.to_string();
            assert!(msg.contains("reads as the tool's own home"), "{msg}");
            assert!(msg.contains("-account"), "it names a usable one: {msg}");
        }
        // Adoption is the same decision, so it refuses the same names.
        let dir = root.path().join("some-home");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(s.adopt("codex", &dir).is_err());
        // Anything that names the ACCOUNT is fine, including a name that merely
        // contains a tool's name.
        assert!(s.create("claude-personal").is_ok());
        assert!(s.create("work").is_ok());
        // And renaming INTO one is refused too - it is the same mistake, later.
        assert!(s.rename("work", "codex").is_err());
        // Renaming a slot that already HAS such a name out of it must work, or
        // an existing install could never be fixed.
        s.records.push(SlotRecord {
            name: "claude".into(),
            id: "legacy".into(),
            config_dir: root.path().join("legacy"),
            adopted: true,
            tool: "claude-code".into(),
        });
        assert!(
            s.rename("claude", "youdie006").unwrap(),
            "the way out works"
        );
    }

    #[test]
    fn a_suggested_name_steps_around_what_is_taken() {
        assert_eq!(suggest_non_colliding("claude", &[]), "claude-account");
        assert_eq!(
            suggest_non_colliding("claude", &["claude-account".into()]),
            "claude-account2"
        );
        assert_eq!(
            suggest_non_colliding("Codex", &["codex-account".into(), "codex-account2".into()]),
            "codex-account3"
        );
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

    #[test]
    fn set_default_points_at_the_slot_dir() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let rec = s.create("work").unwrap();
        assert_eq!(s.default_dir(), None, "no default until set");
        s.set_default("work").unwrap();
        assert_eq!(s.default_dir(), Some(rec.config_dir.clone()));
        // Re-open sees the same pointer (persisted on disk).
        assert_eq!(
            Slots::open(&paths).unwrap().default_dir(),
            Some(rec.config_dir)
        );
        assert!(s.set_default("missing").is_err(), "unknown name rejected");
    }

    #[test]
    fn rename_keeps_the_directory_so_the_login_survives() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let before = s.create("company2").unwrap();
        assert!(s.rename("company2", "rnd").unwrap());
        let after = s.get("rnd").expect("renamed");
        assert_eq!(
            after.config_dir, before.config_dir,
            "the directory is untouched - the Keychain item is keyed on it"
        );
        assert_eq!(after.id, before.id, "and so is the id");
        assert!(s.get("company2").is_none());
        // Colliding and missing names are refused rather than silently applied.
        s.create("other").unwrap();
        assert!(s.rename("rnd", "other").is_err(), "duplicate refused");
        assert!(
            !s.rename("   ", "x").unwrap_or(false),
            "a blank name is not a rename"
        );
        assert!(!s.rename("ghost", "x").unwrap(), "no such slot");
    }

    #[test]
    fn remove_unregisters_but_never_deletes_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let rec = s.create("work").unwrap();
        s.set_default("work").unwrap();
        assert!(s.remove("work").unwrap());
        assert!(s.get("work").is_none(), "the mapping is gone");
        assert!(
            rec.config_dir.is_dir(),
            "the directory - and the login in it - is left alone"
        );
        assert_eq!(
            s.default_dir(),
            None,
            "a pointer at the removed slot is cleared, not left dangling"
        );
        // Gone means gone, and a second removal is not an error.
        assert!(!s.remove("work").unwrap());
        // It survives a reopen.
        assert!(Slots::open(&paths).unwrap().get("work").is_none());
    }

    #[test]
    fn adopt_registers_an_existing_dir_without_moving_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let existing = root.path().join("dot-claude-company");
        std::fs::create_dir_all(&existing).unwrap();
        let mut s = Slots::open(&paths).unwrap();
        let rec = s.adopt("company", &existing).unwrap();
        assert_eq!(rec.config_dir, existing, "config dir is the existing path");
        assert!(rec.adopted);
        assert!(existing.is_dir(), "the existing dir is left in place");
        // A non-existent dir is refused.
        assert!(s.adopt("nope", &root.path().join("absent")).is_err());
    }
}

#[cfg(test)]
mod adopt_history_tests {
    use super::*;

    fn write(p: &std::path::Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// Turning an isolated history into a shared one must not lose a
    /// conversation. Anything the slot has and the shared store does not is
    /// carried over; anything already there is left alone, because the shared
    /// copy is the one every account can see.
    #[test]
    fn conversations_only_the_slot_had_are_carried_over() {
        let t = tempfile::tempdir().unwrap();
        let shared = t.path().join("bare/projects");
        let slot = t.path().join("slot/projects");
        write(&shared.join("projA/one.jsonl"), "shared-one");
        write(&slot.join("projA/two.jsonl"), "slot-two");
        write(&slot.join("projB/three.jsonl"), "slot-three");
        // Same path in both: the shared one wins and is not overwritten.
        write(&slot.join("projA/one.jsonl"), "slot-version");

        let moved = carry_history_into_shared(&slot, &shared, false).unwrap();
        assert_eq!(moved, 2, "two files were only in the slot");
        assert_eq!(
            std::fs::read_to_string(shared.join("projA/one.jsonl")).unwrap(),
            "shared-one",
            "an existing shared conversation is never overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(shared.join("projA/two.jsonl")).unwrap(),
            "slot-two"
        );
        assert_eq!(
            std::fs::read_to_string(shared.join("projB/three.jsonl")).unwrap(),
            "slot-three"
        );
    }

    /// The slot's own copies stay where they are. Nothing is deleted by this
    /// step - the caller decides what to do with the directory afterwards, and
    /// a half-finished merge must leave the originals readable.
    #[test]
    fn the_slots_own_copies_are_left_in_place() {
        let t = tempfile::tempdir().unwrap();
        let shared = t.path().join("bare/projects");
        let slot = t.path().join("slot/projects");
        write(&slot.join("p/a.jsonl"), "x");
        carry_history_into_shared(&slot, &shared, false).unwrap();
        assert!(
            slot.join("p/a.jsonl").exists(),
            "originals survive the copy"
        );
    }

    /// A preview that changes what it previews is worse than none: its answer
    /// is only true the first time, and the second run reports zero because the
    /// first already did the work. Caught on a real machine, where `--dry-run`
    /// said "4 conversations" and then the real run said "0".
    #[test]
    fn a_dry_run_counts_and_writes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let shared = t.path().join("bare/projects");
        let slot = t.path().join("slot/projects");
        write(&slot.join("p/a.jsonl"), "x");
        write(&slot.join("p/b.jsonl"), "y");

        assert_eq!(carry_history_into_shared(&slot, &shared, true).unwrap(), 2);
        assert!(!shared.exists(), "a dry run creates nothing");
        // And it is repeatable: the same answer every time it is asked.
        assert_eq!(carry_history_into_shared(&slot, &shared, true).unwrap(), 2);
    }

    #[test]
    fn a_slot_with_no_history_carries_nothing() {
        let t = tempfile::tempdir().unwrap();
        let shared = t.path().join("bare/projects");
        assert_eq!(
            carry_history_into_shared(&t.path().join("slot/projects"), &shared, false).unwrap(),
            0
        );
    }
}

#[cfg(test)]
mod rename_any_tool_tests {
    use super::*;
    use crate::paths::Paths;

    /// Renaming looked the account up in the CLAUDE registry only, so a Codex
    /// slot was never found: the snapshot was renamed and the slot kept its old
    /// name. Seen on a real machine, where `ls` said `codex` and the registry
    /// still said `A`.
    #[test]
    fn a_slot_of_any_tool_can_be_found_by_name() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(t.path());
        let mut codex = Slots::open_for(&paths, "codex").unwrap();
        codex.create("A").unwrap();

        assert!(
            find_any_tool(&paths, "A").is_some(),
            "a codex slot must be findable without naming its tool"
        );
        assert!(find_any_tool(&paths, "nope").is_none());

        let mut claude = Slots::open_for(&paths, "claude-code").unwrap();
        claude.create("bsgong").unwrap();
        assert_eq!(
            find_any_tool(&paths, "bsgong").map(|(t, _)| t),
            Some("claude-code".to_string()),
            "and a claude slot still resolves to its own tool"
        );
        assert_eq!(
            find_any_tool(&paths, "A").map(|(t, _)| t),
            Some("codex".to_string())
        );
    }
}
