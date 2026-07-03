//! One `AuthTool` per supported CLI. An adapter knows where a tool keeps its
//! login on disk and how to capture/apply/inspect it. Secrets live only inside
//! `Snapshot` (as `Secret`); `Account` is the redacted, serializable identity.

use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::Result;
use serde::Serialize;

mod claude;
mod codex;

/// A tool's live account identity, safe to print/serialize - no tokens.
#[derive(Serialize, Clone)]
pub struct Account {
    pub tool: &'static str,
    pub account_id: String,
    pub display: String,
    pub email: Option<String>,
    pub tier: Option<String>,
    pub expires_at: Option<i64>,
}

/// A captured login: named opaque parts, each a `Secret`. Claude has
/// ("credentials", ..) + ("oauth_account", ..); Codex has ("auth", ..).
pub struct Snapshot {
    pub tool: &'static str,
    pub blobs: Vec<(String, Secret)>,
}

impl Snapshot {
    pub fn part(&self, name: &str) -> Option<&Secret> {
        self.blobs.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }
}

pub trait AuthTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn present(&self, paths: &Paths) -> bool;
    fn capture(&self, paths: &Paths) -> Result<Snapshot>;
    fn apply(&self, paths: &Paths, snap: &Snapshot) -> Result<()>;
    fn identity(&self, paths: &Paths) -> Result<Option<Account>>;
}

pub fn all() -> Vec<Box<dyn AuthTool>> {
    vec![Box::new(claude::Claude), Box::new(codex::Codex)]
}

pub fn by_name(name: &str) -> Option<Box<dyn AuthTool>> {
    all().into_iter().find(|a| a.name() == name)
}
