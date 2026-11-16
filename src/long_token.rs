//! A Claude account's one-year token, from `claude setup-token`.
//!
//! A Claude sign-in ends about 27 days after it is made, and renewing does not
//! move that end (see `refresh::LOGIN_END_NOTICE_MS`). `claude setup-token`
//! issues a token that lasts a year instead, for model requests only. swapdex
//! keeps one beside the account and, as the sign-in's end arrives, puts it where
//! the login was, in the shape Claude Code itself builds for
//! CLAUDE_CODE_OAUTH_TOKEN: no refresh token and no expiry. Nothing then tries
//! to renew it, the proxy serves with it, and a session in the account keeps
//! going. claude.ai connectors and Remote Control need the full sign-in, so
//! they stop until the next /login replaces the token.

use crate::paths::Paths;
use crate::refresh::LoginEnd;
use crate::secret::Secret;
use anyhow::Context as _;
use std::path::Path;
use zeroize::Zeroize as _;

pub const FILE: &str = ".swapdex-long-token.json";
const VERSION: u8 = 1;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `claude setup-token` asks for a year (31536000 s in the 2.1.325 binary).
/// The token does not say when it was made, so the year is counted from when
/// swapdex was given it, and said as "about".
pub const LIFETIME_MS: i64 = 365 * DAY_MS;

/// How long before the sign-in's end the token takes over. Longer than the
/// proxy's half-hourly sweep, so some sweep always lands before the first
/// renewal that the server would refuse.
pub const TAKE_OVER_LEAD_MS: i64 = 45 * 60 * 1000;
const _: () = assert!(TAKE_OVER_LEAD_MS as u128 > crate::proxy::KEEP_ALIVE_EVERY.as_millis());

/// How close the token's own end is before `ls` names it.
pub const END_NOTICE_MS: i64 = 30 * DAY_MS;

#[derive(serde::Serialize, serde::Deserialize)]
struct Stored {
    version: u8,
    token: String,
    saved_at_ms: i64,
}

pub struct LongToken {
    token: Secret,
    pub saved_at_ms: i64,
}

impl LongToken {
    pub fn ends_at_ms(&self) -> i64 {
        self.saved_at_ms.saturating_add(LIFETIME_MS)
    }

    fn token(&self) -> &str {
        std::str::from_utf8(self.token.expose()).unwrap_or_default()
    }
}

/// Whether `token` looks like what `claude setup-token` prints.
pub fn well_formed(token: &str) -> bool {
    token.starts_with("sk-ant-oat")
        && (40..=512).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn save(dir: &Path, token: &str, now_ms: i64) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(&Stored {
        version: VERSION,
        token: token.to_string(),
        saved_at_ms: now_ms,
    })?;
    let written = crate::atomic::write_secret(&dir.join(FILE), &bytes);
    bytes.zeroize();
    written
}

pub fn load(dir: &Path) -> Option<LongToken> {
    let mut bytes = crate::atomic::read_regular(&dir.join(FILE)).ok()?;
    let stored = serde_json::from_slice::<Stored>(&bytes).ok();
    bytes.zeroize();
    let stored = stored?;
    (stored.version == VERSION && well_formed(&stored.token)).then(|| LongToken {
        token: Secret::new(stored.token.into_bytes()),
        saved_at_ms: stored.saved_at_ms,
    })
}

/// The saved token while its year lasts. Past it the token is refused, and
/// putting it in place of a login would only trade one refusal for another.
pub fn current(dir: &Path, now_ms: i64) -> Option<LongToken> {
    load(dir).filter(|long| long.ends_at_ms() > now_ms)
}

/// Forget the saved token. A store it already stands in keeps it until a
/// /login replaces it; taking it out would sign the account out.
pub fn remove(dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(dir.join(FILE)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `old` with its login replaced by `token`, as Claude Code builds one for
/// CLAUDE_CODE_OAUTH_TOKEN. The rest of the store - MCP servers' logins - and
/// the plan fields Claude Code shows are kept.
pub fn credential_with(old: &[u8], token: &str) -> Option<Vec<u8>> {
    let mut store: serde_json::Value = serde_json::from_slice(old).ok()?;
    let fields = store.as_object_mut()?;
    let before = fields.get("claudeAiOauth").cloned().unwrap_or_default();
    fields.insert(
        "claudeAiOauth".into(),
        serde_json::json!({
            "accessToken": token,
            "refreshToken": null,
            "expiresAt": null,
            "scopes": ["user:inference"],
            "subscriptionType": before["subscriptionType"],
            "rateLimitTier": before["rateLimitTier"],
        }),
    );
    serde_json::to_vec(&store).ok()
}

/// Whether the store already holds this token in place of a login.
pub fn installed(blob: &[u8], long: &LongToken) -> bool {
    serde_json::from_slice::<serde_json::Value>(blob)
        .is_ok_and(|v| v["claudeAiOauth"]["accessToken"].as_str() == Some(long.token()))
}

/// Whether the sign-in in this store has reached, or is about to reach, its
/// fixed end. Claude Code marks one it was refused for as `loginEnded:
/// "time_limit"`. A store with no login at all was signed out on purpose and
/// is left alone.
pub fn due(blob: &[u8], now_ms: i64) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(blob) else {
        return false;
    };
    let oauth = &v["claudeAiOauth"];
    oauth["loginEnded"].as_str() == Some("time_limit")
        || oauth["refreshTokenExpiresAt"]
            .as_i64()
            .is_some_and(|end| end - now_ms <= TAKE_OVER_LEAD_MS)
}

#[derive(Debug, PartialEq, Eq)]
pub enum TakeOver {
    NotSaved,
    NotDue,
    Already,
    Done,
}

/// Put the saved token in place of this account's login if its sign-in's end
/// has come. Done under Claude Code's own refresh lock: Claude Code rereads the
/// store under it before renewing, so a renewal racing this one either lands
/// first, and is read below, or finds the token and has nothing to renew.
pub fn take_over(paths: &Paths, dir: &Path, now_ms: i64) -> anyhow::Result<TakeOver> {
    let Some(long) = current(dir, now_ms) else {
        return Ok(TakeOver::NotSaved);
    };
    let read = |authority: &crate::claude_authority::Authority| {
        authority
            .read(paths)
            .map_err(|_| anyhow::anyhow!("its Claude login could not be read"))
    };
    let authority = crate::claude_authority::resolve(paths, dir)?;
    let before = read(&authority)?;
    if installed(before.bytes(), &long) {
        return Ok(TakeOver::Already);
    }
    if !due(before.bytes(), now_ms) {
        return Ok(TakeOver::NotDue);
    }
    let lock = crate::refresh::claude_lock(&authority.storage_dir)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("Claude's refresh lock")?;
    let current = read(&authority)?;
    if installed(current.bytes(), &long) {
        return Ok(TakeOver::Already);
    }
    if !due(current.bytes(), now_ms) {
        return Ok(TakeOver::NotDue);
    }
    let mut replaced = credential_with(current.bytes(), long.token())
        .context("its Claude login is not a credential swapdex can read")?;
    lock.ensure_owned()
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("Claude's refresh lock")?;
    let written = authority.write(paths, current.source(), &replaced);
    replaced.zeroize();
    written.map(|()| TakeOver::Done)
}

/// `take_over` for every Claude account, as (name, outcome) for the log.
pub fn take_over_all(paths: &Paths, now_ms: i64) -> Vec<(String, anyhow::Result<TakeOver>)> {
    let Ok(slots) = crate::slots::Slots::open_for(paths, "claude-code") else {
        return Vec::new();
    };
    slots
        .list()
        .into_iter()
        .filter(|r| current(&r.config_dir, now_ms).is_some())
        .map(|r| (r.name, take_over(paths, &r.config_dir, now_ms)))
        .collect()
}

fn date(at_ms: i64, tz_offset: i64) -> String {
    let (y, m, d) = crate::codex_tls::civil(at_ms / 1000 + tz_offset);
    format!("{y}-{m:02}-{d:02}")
}

/// The login-end note for an account whose token will take over: nothing has
/// to be done by then, so it says what will happen instead.
pub fn covered_end_note(end: &LoginEnd, now_ms: i64, tz_offset: i64) -> String {
    let clock = |at: i64| crate::proxy::pick::clock_at(at / 1000, now_ms / 1000, tz_offset);
    match end {
        LoginEnd::Ends(at) => format!(
            "login ends {} - its 1-year token takes over then",
            clock(*at)
        ),
        LoginEnd::Ended(at) => format!(
            "login ended {} - its 1-year token takes over within half an hour",
            clock(*at)
        ),
    }
}

/// What `ls` and `doctor` say about an account's saved token, and whether it
/// asks for something to be done: its year nearly up does, running on it does
/// not. Nothing otherwise.
pub fn status_note(paths: &Paths, dir: &Path, name: &str, now_ms: i64) -> Option<(String, bool)> {
    let long = load(dir)?;
    let tz = crate::proxy::tz_offset();
    let ends = date(long.ends_at_ms(), tz);
    let renew = format!(
        "`claude setup-token`, then `swapdex token {}`",
        crate::util::shell_word(name)
    );
    if long.ends_at_ms() <= now_ms {
        return Some((format!("1-year token ended about {ends} - {renew}"), true));
    }
    if long.ends_at_ms() - now_ms <= END_NOTICE_MS {
        return Some((format!("1-year token ends about {ends} - {renew}"), true));
    }
    let in_use =
        crate::claude_authority::credential(paths, dir).is_ok_and(|c| installed(c.bytes(), &long));
    in_use.then(|| {
        let note = format!(
            "running on its 1-year token until about {ends} - /login in a `swapdex run {}` \
             session brings back claude.ai connectors",
            crate::util::shell_word(name)
        );
        (note, false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const TOKEN: &str = "sk-ant-oat01-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn login(refresh_end: Option<i64>) -> Vec<u8> {
        let mut oauth = serde_json::json!({
            "accessToken": "sk-ant-oat01-short-lived",
            "refreshToken": "sk-ant-ort01-refresh",
            "expiresAt": NOW + 8 * 3_600_000,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
            "rateLimitTier": "default_claude_max_20x",
        });
        if let Some(end) = refresh_end {
            oauth["refreshTokenExpiresAt"] = end.into();
        }
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": oauth,
            "mcpOAuth": {"server": {"accessToken": "mcp-token"}},
        }))
        .unwrap()
    }

    #[test]
    fn only_a_setup_token_is_taken() {
        assert!(well_formed(TOKEN));
        assert!(!well_formed(
            "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ));
        assert!(!well_formed("sk-ant-oat01-short"));
        assert!(!well_formed(&format!("{TOKEN} trailing")));
        assert!(!well_formed(&format!("{TOKEN}\"")));
    }

    #[test]
    fn the_token_stands_where_the_login_was_and_the_rest_is_kept() {
        let replaced = credential_with(&login(Some(NOW)), TOKEN).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&replaced).unwrap();
        let oauth = &v["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], TOKEN);
        assert!(oauth["refreshToken"].is_null(), "nothing left to renew");
        assert!(
            oauth["expiresAt"].is_null(),
            "Claude Code reads null as never"
        );
        assert!(oauth.get("refreshTokenExpiresAt").is_none());
        assert_eq!(oauth["scopes"], serde_json::json!(["user:inference"]));
        assert_eq!(oauth["subscriptionType"], "max");
        assert_eq!(oauth["rateLimitTier"], "default_claude_max_20x");
        assert_eq!(v["mcpOAuth"]["server"]["accessToken"], "mcp-token");
    }

    #[test]
    fn the_token_takes_over_only_as_the_sign_in_ends() {
        assert!(
            !due(&login(None), NOW),
            "no stated end, nothing to wait for"
        );
        assert!(!due(&login(Some(NOW + 3 * 3_600_000)), NOW));
        assert!(!due(&login(Some(NOW + TAKE_OVER_LEAD_MS + 1)), NOW));
        assert!(due(&login(Some(NOW + TAKE_OVER_LEAD_MS)), NOW));
        assert!(due(&login(Some(NOW - 1)), NOW));
        let marked = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {"accessToken": "", "refreshToken": "", "loginEnded": "time_limit"}
        }))
        .unwrap();
        assert!(due(&marked, NOW), "Claude Code was already refused");
        assert!(!due(b"{}", NOW), "signed out on purpose");
        assert!(!due(b"not json", NOW));
    }

    #[test]
    fn a_saved_token_reads_back_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), TOKEN, NOW).unwrap();
        let long = load(dir.path()).unwrap();
        assert_eq!(long.token(), TOKEN);
        assert_eq!(long.ends_at_ms(), NOW + LIFETIME_MS);
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path().join(FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        assert!(remove(dir.path()).unwrap());
        assert!(load(dir.path()).is_none());
        assert!(!remove(dir.path()).unwrap());
    }

    #[test]
    fn a_token_past_its_year_does_not_take_over() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), TOKEN, NOW - LIFETIME_MS).unwrap();
        assert!(load(dir.path()).is_some());
        assert!(current(dir.path(), NOW).is_none());
        assert!(current(dir.path(), NOW - 1).is_some());
        let paths = Paths::rooted(dir.path());
        assert_eq!(
            take_over(&paths, dir.path(), NOW).unwrap(),
            TakeOver::NotSaved
        );
    }

    #[test]
    fn a_token_installed_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), TOKEN, NOW).unwrap();
        let long = load(dir.path()).unwrap();
        assert!(!installed(&login(Some(NOW)), &long));
        assert!(installed(
            &credential_with(&login(Some(NOW)), TOKEN).unwrap(),
            &long
        ));
    }

    #[test]
    fn the_note_says_what_happens_instead_of_what_to_do() {
        let ends = covered_end_note(&LoginEnd::Ends(NOW + 3_600_000), NOW, 0);
        assert!(ends.contains("takes over then"), "{ends}");
        assert!(!ends.contains("/login"), "{ends}");
        let ended = covered_end_note(&LoginEnd::Ended(NOW - 1), NOW, 0);
        assert!(ended.contains("takes over"), "{ended}");
    }
}
