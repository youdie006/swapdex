use super::{Account, AuthTool, Snapshot};
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Codex;

impl AuthTool for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn present(&self, paths: &Paths) -> bool {
        paths.codex_auth().exists()
    }

    fn capture(&self, paths: &Paths) -> Result<Snapshot> {
        let path = paths.codex_auth();
        if !path.exists() {
            bail!("not logged in to Codex (no {})", path.display());
        }
        let bytes = crate::atomic::read_regular(&path)?;
        // Validate before accepting, so a torn read is never stored.
        serde_json::from_slice::<Value>(&bytes).context("codex auth.json is not valid JSON")?;
        Ok(Snapshot {
            tool: "codex",
            blobs: vec![("auth".into(), Secret::new(bytes))],
        })
    }

    fn apply(&self, paths: &Paths, snap: &Snapshot) -> Result<()> {
        let part = snap.part("auth").context("snapshot has no codex auth")?;
        crate::atomic::write_secret(&paths.codex_auth(), part.expose())
    }

    fn identity(&self, paths: &Paths) -> Result<Option<Account>> {
        let path = paths.codex_auth();
        if !path.exists() {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&crate::atomic::read_regular(&path)?)
            .context("parse codex auth.json")?;
        let account_id = v["tokens"]["account_id"].as_str().unwrap_or("").to_string();
        let email = decode_email_from_id_token(v["tokens"]["id_token"].as_str());
        Ok(Some(Account {
            tool: "codex",
            account_id,
            display: email.clone().unwrap_or_else(|| "codex account".into()),
            email,
            tier: v["auth_mode"].as_str().map(|s| s.to_string()),
            expires_at: None,
        }))
    }
}

/// Extract only the `email` claim from a JWT id_token payload - never keep the
/// token. Best-effort; None on any decode failure.
fn decode_email_from_id_token(id_token: Option<&str>) -> Option<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let payload = id_token?.split('.').nth(1)?;
    let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: Value = serde_json::from_slice(&json).ok()?;
    v["email"].as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    fn seed_codex(p: &Paths, account_id: &str) {
        let dir = p.codex_auth().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": "sk-SENTINELKEY",
            "tokens": {"id_token": "hdr.eyJlbWFpbCI6ImFAYi5jb20ifQ.sig",
                       "access_token": "AT-SENTINEL", "refresh_token": "RT-SENTINEL",
                       "account_id": account_id},
            "last_refresh": "2026-07-03T00:00:00Z"
        });
        std::fs::write(p.codex_auth(), serde_json::to_vec(&body).unwrap()).unwrap();
    }

    #[test]
    fn codex_capture_apply_roundtrip_and_identity() {
        let a = tempfile::tempdir().unwrap();
        let pa = Paths::rooted(a.path());
        seed_codex(&pa, "acct-123");
        let snap = Codex.capture(&pa).unwrap();

        let b = tempfile::tempdir().unwrap();
        let pb = Paths::rooted(b.path());
        Codex.apply(&pb, &snap).unwrap();
        assert_eq!(
            std::fs::read(pb.codex_auth()).unwrap(),
            std::fs::read(pa.codex_auth()).unwrap(),
            "whole auth.json round-trips byte-for-byte"
        );
        let id = Codex.identity(&pb).unwrap().unwrap();
        assert_eq!(id.account_id, "acct-123");
        assert_eq!(id.tool, "codex");
        assert_eq!(id.email.as_deref(), Some("a@b.com"));
    }
}
