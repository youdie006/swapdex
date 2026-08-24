//! Reading a slot's own credential. The slot is the single source of truth: the
//! token is held in memory for one request and never written anywhere else.

use crate::secret::Secret;
use std::path::Path;

/// The access token stored in this slot, or `None` when the slot has no readable
/// login. Linux/WSL: the slot's `.credentials.json`. macOS: the slot's own
/// Keychain item, whose service name is derived from the config dir the way
/// Claude Code derives it.
pub fn slot_token(dir: &Path) -> Option<Secret> {
    slot_token_detail(dir).ok()
}

/// Why a slot's token could not be read, phrased as the thing the user should do.
/// A LOCKED Keychain is the case worth separating: the account is signed in and
/// the environment is what is wrong (a non-interactive ssh session cannot read a
/// secret from the login keychain), so telling the user to sign in again would
/// send them to fix something that is not broken.
pub fn slot_token_detail(dir: &Path) -> Result<Secret, TokenUnavailable> {
    if let Ok(bytes) = std::fs::read(dir.join(".credentials.json")) {
        if let Some(t) = access_token(&bytes) {
            return Ok(t);
        }
    }
    use crate::adapters::claude::KeychainReadError as K;
    match crate::adapters::claude::slot_keychain_read_detail(dir) {
        Ok(bytes) => access_token(&bytes).ok_or(TokenUnavailable::NoLogin),
        Err(K::Locked) => Err(TokenUnavailable::KeychainLocked),
        Err(K::Missing | K::NotApplicable) => Err(TokenUnavailable::NoLogin),
    }
}

pub enum TokenUnavailable {
    /// The slot has never been signed into (or its login is unreadable).
    NoLogin,
    /// macOS: the login exists but the keychain will not release it here.
    KeychainLocked,
}

impl TokenUnavailable {
    /// The one next step, for the account named `name`.
    pub fn remedy(&self, name: &str) -> String {
        match self {
            Self::NoLogin => format!(
                "account '{name}' has no usable login - `swapdex run {name}` once signs it in"
            ),
            Self::KeychainLocked => format!(
                "account '{name}' is signed in, but macOS will not release its login here: \
                 reading a Keychain secret needs an unlocked login keychain, which a remote \
                 or non-interactive shell does not have. Run the proxy from a terminal on \
                 the Mac itself (or unlock with `security unlock-keychain`)."
            ),
        }
    }

    /// The short reason, for a list that has one line per account. `remedy` is
    /// the full paragraph; this is what fits next to a name.
    pub fn short(&self) -> &'static str {
        match self {
            Self::NoLogin => "no saved token",
            // NOT "no saved token". The login is there; this shell cannot read
            // it. Saying otherwise sends someone to re-sign-in an account that
            // was signed in the whole time.
            Self::KeychainLocked => "signed in, but this shell cannot read the keychain",
        }
    }
}

#[cfg(test)]
mod unavailable_tests {
    use super::*;

    /// A locked keychain and an account with no login must never read the same.
    /// Over ssh every macOS account reports as tokenless, and three separate
    /// times this session that output was nearly taken for the truth about
    /// 병승's accounts.
    #[test]
    fn a_locked_keychain_does_not_read_as_a_missing_login() {
        assert_eq!(TokenUnavailable::NoLogin.short(), "no saved token");
        let locked = TokenUnavailable::KeychainLocked.short();
        assert!(
            !locked.contains("no saved token"),
            "a locked keychain must not read as an absent login: {locked}"
        );
        assert!(
            locked.contains("signed in"),
            "it says the login is fine: {locked}"
        );
    }
}

/// Has this slot's access token already expired? Sending an expired token just
/// earns a 401, and nothing here can refresh it - Claude refreshes its OWN
/// token, not one the proxy injected - so an expired slot must be stepped over
/// rather than tried. `false` when the expiry cannot be read (macOS keeps the
/// credential in the Keychain): unknown is not the same as expired.
/// A minute of slack: a token about to lapse mid-flight is already useless.
const SLACK_MS: i64 = 60_000;

/// The verdict, given what each store says the expiry is.
///
/// Two stores can both hold a credential, and they disagree: on macOS
/// `.credentials.json` is a LEFTOVER - Claude Code keeps the real one in the
/// Keychain - so reading the file first reported a slot signed in minutes ago
/// as expired on the strength of a file three days old. The owner logged in,
/// `ls` still said `(expired)`, and nothing they did could change it.
///
/// Whichever blob expires LATER is the one that would actually authenticate, so
/// that is the one the verdict follows. Neither speaking means nothing is
/// known, and nothing is claimed.
pub fn expired_from(from_file: Option<i64>, from_keychain: Option<i64>, now_ms: i64) -> bool {
    match from_file.into_iter().chain(from_keychain).max() {
        Some(exp) => exp - now_ms <= SLACK_MS,
        None => false,
    }
}

pub fn slot_token_expired(dir: &Path, now_ms: i64) -> bool {
    let expiry_of = |b: Vec<u8>| -> Option<i64> {
        serde_json::from_slice::<serde_json::Value>(&b)
            .ok()?
            .get("claudeAiOauth")?
            .get("expiresAt")?
            .as_i64()
    };
    // Ask BOTH stores. Asking the file first and the Keychain only when the
    // file was absent let a leftover outvote the credential in use.
    let from_file = std::fs::read(dir.join(".credentials.json"))
        .ok()
        .and_then(expiry_of);
    let from_keychain = crate::adapters::claude::slot_keychain_read_detail(dir)
        .ok()
        .and_then(expiry_of);
    expired_from(from_file, from_keychain, now_ms)
}

/// This slot's own account UUID, from its `.claude.json` `oauthAccount` - the
/// identity Claude reports as "connected". Used to keep a forwarded request's
/// `metadata.user_id` consistent with the token serving it. An identifier, not a
/// secret, and never logged.
pub fn slot_account_uuid(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(".claude.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["oauthAccount"]["accountUuid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// This slot's connected email, from its `.claude.json` - a label, not a secret.
pub fn slot_email(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(".claude.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["oauthAccount"]["emailAddress"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// This slot's email, whatever tool wrote it.
///
/// `slot_email` reads `.claude.json` only, so a Codex slot listed with an empty
/// name column - the same defect fixed for Claude in 0.82.0, still live on the
/// other side because Codex keeps its identity inside an id_token in
/// `auth.json` instead. A caller that just wants "whose login is this" should
/// not have to know which tool the slot belongs to.
pub fn any_slot_email(dir: &Path) -> Option<String> {
    if let Some(e) = slot_email(dir) {
        return Some(e);
    }
    let bytes = std::fs::read(dir.join("auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    crate::adapters::codex::decode_email_from_id_token(v["tokens"]["id_token"].as_str())
}

/// Does the identity recorded for this slot disagree with the login it holds?
///
/// The name beside an account comes from `.claude.json`, the numbers come from
/// the credential, and nothing keeps them in step - so a config whose identity
/// was overwritten shows one person's name above another person's usage. Only a
/// contradiction is reported, never a guess: an identity that names an
/// organisation beside a personal subscription cannot both be true.
pub fn identity_contradicts_login(dir: &Path) -> Option<String> {
    let id = std::fs::read(dir.join(".claude.json")).ok()?;
    let id: serde_json::Value = serde_json::from_slice(&id).ok()?;
    let email = id["oauthAccount"]["emailAddress"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    let org = id["oauthAccount"]["organizationName"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    // A personal account still carries an organisation - Anthropic names one
    // after the address itself. Treating any organisation as proof of a team
    // account made every personal login look like a contradiction.
    if org.contains(email) {
        return None;
    }
    let blob = slot_token_blob(dir)?;
    let cred: serde_json::Value = serde_json::from_slice(&blob).ok()?;
    let sub = cred["claudeAiOauth"]["subscriptionType"].as_str()?;
    // Worth pointing at, NOT proof. This used to read "the name and the login
    // belong to different accounts", which the data cannot support: a person in
    // an organisation may hold a personal Max or Pro plan, and that is an
    // ordinary setup rather than a fault. The credential carries a plan name and
    // scopes - no account identifier at all - so nothing here can tell one
    // account from two. What it CAN do is say what it sees and name the one
    // check that settles it.
    if matches!(sub, "max" | "pro") {
        return Some(format!(
            "recorded as {email} ({org}), and its credential is a '{sub}' plan. \
             That is normal if you hold a personal plan alongside the \
             organisation; if you did not expect it, the config was written by a \
             different login than the one in this slot - `swapdex whoami` while \
             running as this account settles it"
        ));
    }
    None
}

/// The credential blob for this slot, wherever it lives.
fn slot_token_blob(dir: &Path) -> Option<Vec<u8>> {
    if let Ok(b) = std::fs::read(dir.join(".credentials.json")) {
        if !b.is_empty() {
            return Some(b);
        }
    }
    crate::adapters::claude::slot_keychain_read_detail(dir).ok()
}

/// Pull `claudeAiOauth.accessToken` out of a Claude credential blob.
fn access_token(bytes: &[u8]) -> Option<Secret> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let t = v["claudeAiOauth"]["accessToken"].as_str()?;
    (!t.is_empty()).then(|| Secret::new(t.as_bytes().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_token_reads_the_access_token_from_the_slot_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(slot_token(dir.path()).is_none(), "no login yet");
        std::fs::write(
            dir.path().join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"AT-1","refreshToken":"RT-1","expiresAt":1}}"#,
        )
        .unwrap();
        let t = slot_token(dir.path()).expect("token");
        assert_eq!(t.expose(), b"AT-1");
    }

    #[test]
    fn slot_account_uuid_comes_from_the_slots_oauth_account() {
        let dir = tempfile::tempdir().unwrap();
        assert!(slot_account_uuid(dir.path()).is_none(), "no config yet");
        std::fs::write(
            dir.path().join(".claude.json"),
            br#"{"oauthAccount":{"accountUuid":"u-1","emailAddress":"a@x.com"}}"#,
        )
        .unwrap();
        assert_eq!(slot_account_uuid(dir.path()).as_deref(), Some("u-1"));
        // An empty or absent uuid must not become an identity.
        std::fs::write(
            dir.path().join(".claude.json"),
            br#"{"oauthAccount":{"accountUuid":""}}"#,
        )
        .unwrap();
        assert!(slot_account_uuid(dir.path()).is_none());
    }

    #[test]
    fn an_expired_slot_is_recognised_and_an_unknown_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000_000i64;
        // Unknown expiry (no file, or a Keychain-backed slot) is NOT "expired":
        // stepping over an account we simply cannot read would be a guess.
        assert!(!slot_token_expired(dir.path(), now));
        let write = |exp: i64| {
            std::fs::write(
                dir.path().join(".credentials.json"),
                format!(r#"{{"claudeAiOauth":{{"accessToken":"A","expiresAt":{exp}}}}}"#),
            )
            .unwrap()
        };
        write(now + 3_600_000);
        assert!(!slot_token_expired(dir.path(), now), "an hour left is fine");
        write(now - 1);
        assert!(slot_token_expired(dir.path(), now), "already lapsed");
        write(now + 30_000);
        assert!(
            slot_token_expired(dir.path(), now),
            "about to lapse mid-flight counts as expired"
        );
    }

    // The name comes from one file and the plan from another, and nothing keeps
    // them in step - so a config whose identity was overwritten shows one
    // person's name above another person's usage. Worth pointing at; not proof.
    #[test]
    fn an_identity_that_cannot_match_its_login_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let id = |email: &str, org: &str| {
            std::fs::write(
                dir.path().join(".claude.json"),
                format!(
                    r#"{{"oauthAccount":{{"emailAddress":"{email}","organizationName":"{org}"}}}}"#
                ),
            )
            .unwrap()
        };
        let cred = |sub: &str| {
            std::fs::write(
                dir.path().join(".credentials.json"),
                format!(r#"{{"claudeAiOauth":{{"accessToken":"A","subscriptionType":"{sub}"}}}}"#),
            )
            .unwrap()
        };
        // An organisation identity beside a personal plan: name both halves.
        id("a@company.com", "Acme RnD");
        cred("max");
        let msg = identity_contradicts_login(dir.path()).expect("worth reporting");
        assert!(msg.contains("a@company.com"), "{msg}");
        assert!(msg.contains("Acme RnD"), "{msg}");
        assert!(msg.contains("max"), "{msg}");
        // ...but it must NOT claim to know they are two accounts. A person in an
        // organisation may hold a personal Max or Pro plan, and the credential
        // carries no account identifier - only a plan name and scopes - so
        // nothing here can tell one account from two. Asserting it anyway sent
        // someone to re-sign-in a login that was fine.
        assert!(
            !msg.contains("different accounts"),
            "it states what it sees, it does not conclude: {msg}"
        );
        assert!(
            msg.contains("normal if"),
            "and it says plainly when this is an ordinary setup: {msg}"
        );

        // The same account consistently: nothing to report.
        cred("team");
        assert!(identity_contradicts_login(dir.path()).is_none());
        // A personal identity on a personal plan is not a contradiction either -
        // including the organisation Anthropic names after the address itself,
        // which every personal account has and which is not a team.
        cred("max");
        id("me@gmail.com", "");
        assert!(identity_contradicts_login(dir.path()).is_none());
        id("me@gmail.com", "me@gmail.com's Organization");
        assert!(
            identity_contradicts_login(dir.path()).is_none(),
            "an account named after its own address is not an organisation"
        );
        // And nothing is claimed when either half is missing.
        std::fs::remove_file(dir.path().join(".credentials.json")).unwrap();
        assert!(identity_contradicts_login(dir.path()).is_none());
    }

    #[test]
    fn slot_token_is_none_for_an_unparseable_credential() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"not json").unwrap();
        assert!(slot_token(dir.path()).is_none());
    }
}

/// Why a proxy must not start, given what it could read from every account.
///
/// A proxy that can read nothing still binds the port, still answers, and
/// forwards the CLIENT's own login on every turn - so it looks like it is
/// working while doing nothing it exists to do. That state cost a full day:
/// started from an ssh session where the Keychain was locked, it served for
/// hours without one line saying so.
///
/// Refusing is the better failure. The shim asks for a port and gets none, so
/// the tool runs with no proxy at all - which is exactly the login the user
/// already has, and it works.
pub fn startup_refusal(reads: &[Result<(), TokenUnavailable>]) -> Option<String> {
    if reads.is_empty() || reads.iter().any(Result::is_ok) {
        return None;
    }
    let locked = reads
        .iter()
        .filter(|r| matches!(r, Err(TokenUnavailable::KeychainLocked)))
        .count();
    Some(if locked == reads.len() {
        "every account is signed in, but this shell cannot open the Keychain to read one. \
         A proxy here would forward your own login on every turn and never say so, which is \
         worse than no proxy. Start it from a terminal on the Mac itself, or unlock with \
         `security unlock-keychain`."
            .to_string()
    } else {
        "no account has a readable login, so there is nothing to serve turns with. \
         Sign one in - `swapdex run <name>` - and start the proxy again."
            .to_string()
    })
}

#[cfg(test)]
mod startup_refusal_tests {
    use super::*;

    #[test]
    fn one_readable_login_is_enough_to_start() {
        assert!(startup_refusal(&[Ok(()), Err(TokenUnavailable::NoLogin)]).is_none());
        assert!(startup_refusal(&[Ok(())]).is_none());
    }

    /// A locked Keychain is its own diagnosis - the accounts ARE signed in, and
    /// telling the user to sign in again sends them to fix something that works.
    #[test]
    fn a_locked_keychain_says_so_rather_than_blaming_the_login() {
        let why = startup_refusal(&[
            Err(TokenUnavailable::KeychainLocked),
            Err(TokenUnavailable::KeychainLocked),
        ])
        .expect("refused");
        assert!(why.contains("Keychain"), "{why}");
        assert!(
            why.contains("unlock-keychain"),
            "the fix comes with it: {why}"
        );
        assert!(!why.contains("Sign one in"), "not the wrong remedy: {why}");
    }

    #[test]
    fn nothing_signed_in_asks_for_a_sign_in() {
        let why = startup_refusal(&[Err(TokenUnavailable::NoLogin)]).expect("refused");
        assert!(why.contains("swapdex run"), "{why}");
    }

    /// No accounts at all is not this function's problem to report - starting
    /// with an empty registry is already handled, and refusing here would
    /// duplicate that with a worse message.
    #[test]
    fn an_empty_registry_is_left_to_the_caller() {
        assert!(startup_refusal(&[]).is_none());
    }
}

#[cfg(test)]
mod any_tool_email_tests {
    use super::*;

    /// A slot's email must be readable whatever tool wrote it.
    ///
    /// `slot_email` read `.claude.json` only, so a Codex slot listed with an
    /// empty name column - the same defect fixed for Claude in 0.82.0, still
    /// live on the other side because Codex keeps its identity inside an
    /// id_token in `auth.json` instead.
    #[test]
    fn a_codex_slot_email_is_read_from_its_auth() {
        let d = tempfile::tempdir().unwrap();
        // Claude shape: unchanged.
        std::fs::write(
            d.path().join(".claude.json"),
            br#"{"oauthAccount":{"emailAddress":"c@x.com"}}"#,
        )
        .unwrap();
        assert_eq!(any_slot_email(d.path()).as_deref(), Some("c@x.com"));

        // Codex shape, in a directory with no .claude.json at all.
        let e = tempfile::tempdir().unwrap();
        let tok = crate::adapters::codex::test_id_token("k@x.com");
        std::fs::write(
            e.path().join("auth.json"),
            serde_json::to_vec(&serde_json::json!({"tokens":{"id_token":tok}})).unwrap(),
        )
        .unwrap();
        assert_eq!(any_slot_email(e.path()).as_deref(), Some("k@x.com"));

        // Neither: nothing, rather than a guess.
        let f = tempfile::tempdir().unwrap();
        assert_eq!(any_slot_email(f.path()), None);
    }
}

#[cfg(test)]
mod stale_file_vs_keychain_tests {
    use super::*;

    /// A leftover file must not outvote a live credential.
    ///
    /// `slot_token_expired` read `.credentials.json` first and consulted the
    /// Keychain only when that file was ABSENT. On macOS the file is a
    /// leftover, since Claude Code keeps the real credential in the Keychain,
    /// so a slot signed in minutes ago was reported expired on the strength of
    /// a file three days old. The owner logged in, `ls` still said
    /// `(expired)`, and nothing they could do would change it.
    ///
    /// Whichever blob expires LATER is the one that would actually
    /// authenticate, so that is the one the verdict follows.
    #[test]
    fn the_later_expiry_wins_between_file_and_keychain() {
        // now = 1000. File lapsed long ago, keychain is good for another hour.
        assert!(!expired_from(Some(0), Some(3_600_000), 1000));
        // The reverse: a fresh file and a stale keychain entry.
        assert!(!expired_from(Some(3_600_000), Some(0), 1000));
        // Both lapsed: expired, which is the whole point of the check.
        assert!(expired_from(Some(0), Some(0), 1000));
        // Only one source says anything: it decides.
        assert!(expired_from(Some(0), None, 1000));
        assert!(!expired_from(None, Some(3_600_000), 1000));
        // Neither: nothing is known, so nothing is claimed.
        assert!(!expired_from(None, None, 1000));
        // The minute of slack still applies to whichever wins.
        assert!(expired_from(None, Some(1_030_000), 1_000_000));
    }
}
