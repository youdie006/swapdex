//! Walk real command sequences and check that every screen agrees with the
//! state underneath it.
//!
//! Written after a day in which fifteen defects shipped past 599 unit tests.
//! None of them was a function computing the wrong answer; they were the joins
//! between functions - a helper built and never wired to the screen that needed
//! it, a refresh trapped inside an `if`, a record written inside the retry loop
//! it was meant to summarise, a listing that looked at snapshots and not slots.
//! A unit test cannot see those, because each unit passes.
//!
//! So these tests do what the owner does: run one command, then ask a DIFFERENT
//! command whether it agrees. Where they disagree is where this class of bug
//! lives.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn run(root: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn chmod600(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

/// A logged-in Claude, the state `add` captures from.
fn seed_claude(root: &Path, uuid: &str, email: &str) {
    let d = root.join(".claude");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "accessToken":"AT","refreshToken":"RT","expiresAt":9999999999999i64,
            "subscriptionType":"max"}}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join(".claude.json"),
        serde_json::to_vec(&serde_json::json!({
            "oauthAccount":{"accountUuid":uuid,"emailAddress":email,"displayName":"X"}}))
        .unwrap(),
    )
    .unwrap();
    chmod600(&d.join(".credentials.json"));
    chmod600(&root.join(".claude.json"));
}

/// A registered SLOT: an account that can actually pay for turns. `add` saves a
/// snapshot, which is a different thing - serving reads a slot's own credential
/// directory, so a snapshot cannot serve until it has been run once.
fn seed_slot(root: &Path, name: &str, email: &str) {
    let dir = root.join(".local/share/swapdex/slots").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "accessToken":"AT","refreshToken":"RT","expiresAt":9999999999999i64,
            "subscriptionType":"max"}}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        serde_json::to_vec(&serde_json::json!({
            "oauthAccount":{"accountUuid":name,"emailAddress":email}}))
        .unwrap(),
    )
    .unwrap();
    chmod600(&dir.join(".credentials.json"));

    let reg = root.join(".local/share/swapdex/slots.json");
    let mut rows: Vec<serde_json::Value> = std::fs::read(&reg)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    rows.push(serde_json::json!({
        "name": name, "id": name, "config_dir": dir.to_string_lossy(),
        "adopted": false, "tool": "claude-code"}));
    std::fs::write(&reg, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join(".local/share/swapdex")).unwrap();
    std::fs::write(t.path().join(".local/share/swapdex/onboarded"), b"1").unwrap();
    t
}

/// Every account that a command will ACT on must be one a listing SHOWS.
///
/// `ls` built its rows from saved snapshots only. On a machine whose accounts
/// live as slots, `serve personal` moved the turns correctly and there was no
/// row for `personal` at all - so the mark naming the payer had nowhere to
/// appear and two of three switches looked like they did nothing. The listing
/// and the switch disagreed about which accounts existed.
#[test]
fn everything_serve_accepts_is_something_ls_shows() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");
    seed_claude(root, "uuid-b", "b@example.com");
    run(root, &["add", "beta"]);
    seed_slot(root, "beta", "b@example.com");

    let (listing, _, _) = run(root, &["ls"]);
    for name in ["alpha", "beta"] {
        // Serve accepts it...
        let (_, err, code) = run(root, &["serve", name]);
        assert_ne!(code, 5, "serve rejected '{name}' as unknown: {err}");
        // ...so the listing has to show it.
        assert!(
            listing.contains(name),
            "serve accepts '{name}' but `ls` never lists it:\n{listing}"
        );
    }
}

/// After a switch, the listing must name the account that was switched to.
///
/// `serve rnd` printed "turns -> rnd" and then `ls` starred a different
/// account, the one holding the login on disk, with the paying account named
/// nowhere.
/// Both facts were true and only one was shown, so switching read as not having
/// taken. Its owner concluded exactly that, repeatedly, over a day.
#[test]
fn a_switch_is_visible_in_the_listing_afterwards() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");
    seed_claude(root, "uuid-b", "b@example.com");
    run(root, &["add", "beta"]);
    seed_slot(root, "beta", "b@example.com");

    for name in ["alpha", "beta", "alpha"] {
        run(root, &["serve", name]);
        let (listing, _, _) = run(root, &["ls"]);
        let marked: Vec<&str> = listing.lines().filter(|l| l.contains("pays")).collect();
        assert_eq!(
            marked.len(),
            1,
            "after `serve {name}` exactly one row should be marked as paying:\n{listing}"
        );
        assert!(
            marked[0].contains(name),
            "after `serve {name}` the paying row names someone else:\n{}",
            marked[0]
        );
    }
}

/// `serve --quiet` is what a status bar calls; it must agree with `ls`.
///
/// The status line and the listing read different sources, so they could
/// disagree about who pays - and the bar is the surface people actually watch.
#[test]
fn the_status_bar_source_agrees_with_the_listing() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");
    seed_claude(root, "uuid-b", "b@example.com");
    run(root, &["add", "beta"]);
    seed_slot(root, "beta", "b@example.com");

    for name in ["beta", "alpha"] {
        run(root, &["serve", name]);
        let (quiet, _, _) = run(root, &["serve", "--quiet"]);
        assert!(
            quiet.starts_with(name),
            "the status bar reports '{}' after `serve {name}`",
            quiet.trim()
        );
        let (listing, _, _) = run(root, &["ls"]);
        let paying = listing
            .lines()
            .find(|l| l.contains("pays"))
            .unwrap_or_default();
        assert!(
            paying.contains(name),
            "bar says '{}' but listing marks:\n{paying}",
            quiet.trim()
        );
    }
}

/// A rename must move the account everywhere, for every tool it holds.
///
/// `Slots::open()` is hardcoded to claude, so renaming an account that also
/// held a Codex login moved half of it: the new name worked for one tool and
/// the old name lingered for the other.
#[test]
fn a_rename_moves_the_account_in_every_listing() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "before"]);
    seed_slot(root, "before", "a@example.com");

    let (_, err, code) = run(root, &["rename", "before", "after"]);
    assert_eq!(code, 0, "rename failed: {err}");

    let (listing, _, _) = run(root, &["ls"]);
    assert!(
        listing.contains("after"),
        "the new name is missing after a rename:\n{listing}"
    );
    assert!(
        !listing.contains("before"),
        "the old name survived the rename:\n{listing}"
    );
    // And the renamed account is the one commands act on.
    let (_, err, code) = run(root, &["serve", "after"]);
    assert_ne!(code, 5, "serve does not know the new name: {err}");
    let (_, _, code) = run(root, &["serve", "before"]);
    assert_eq!(code, 5, "serve still answers to the old name");
}

/// Dropping one tool must not take the account with it.
///
/// The slot branch ran before the `--tool` flag was read, so on a name that is
/// both a slot and a profile, `rm kong --tool gemini --yes` unregistered kong
/// entirely instead of dropping one login. The opposite of what was asked.
#[test]
fn dropping_one_tool_leaves_the_account_listed() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");

    // Nothing to drop: reported, and the account is untouched either way.
    run(root, &["rm", "alpha", "--tool", "gemini", "--yes"]);
    let (listing, _, _) = run(root, &["ls"]);
    assert!(
        listing.contains("alpha"),
        "dropping a tool removed the whole account:\n{listing}"
    );
    let (_, err, code) = run(root, &["serve", "alpha"]);
    assert_ne!(code, 5, "the account stopped existing: {err}");
}

/// A wrong name must answer with the names that exist.
///
/// A typo said "no account named 'alicee' - `swapdex ui` lists them", sending
/// the reader to open another screen to read four words.
#[test]
fn a_wrong_name_names_the_alternatives() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);

    let (_, err, code) = run(root, &["serve", "alphaa"]);
    assert_eq!(code, 5, "a typo should not be accepted");
    assert!(
        err.contains("alpha"),
        "the error does not name the account that exists: {err}"
    );
    assert!(
        !err.contains("swapdex ui"),
        "the error still sends the reader to another screen: {err}"
    );
}

/// Every switch is attributable afterwards.
///
/// The timeline held `serve kong` and nothing about the caller, so a paying
/// account that changed without anyone meaning to could not be traced to any of
/// twenty live sessions.
#[test]
fn a_switch_records_who_asked_for_it() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");
    run(root, &["serve", "alpha"]);

    let line = std::fs::read_to_string(root.join(".local/share/swapdex/timeline.jsonl"))
        .unwrap_or_default();
    let last = line.lines().last().unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(last).unwrap_or_default();
    assert!(
        v.get("by").and_then(|b| b.as_str()).is_some(),
        "the switch does not record who asked for it: {last}"
    );
}

/// The listing must say whose login each row is, however the account is stored.
///
/// Identity was read from the saved snapshot only, so a slot-only account
/// listed with an empty name column: the row was there and switching worked,
/// but nothing said which login it was.
#[test]
fn every_listed_account_says_whose_login_it_is() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "alpha@example.com");
    run(root, &["add", "alpha"]);

    let (listing, _, _) = run(root, &["ls"]);
    let row = listing
        .lines()
        .find(|l| l.contains("alpha "))
        .unwrap_or_default();
    assert!(
        row.contains("alpha@example.com"),
        "the row does not name the login it holds:\n{row}"
    );
}

/// A row must not claim a login it does not hold.
///
/// `ls` marked a freshly created Codex slot as the active account (`codex*`)
/// while `serve` refused it for having no codex login - the slot directory held
/// config and sessions but no `auth.json`. One screen asserted what the other
/// denied, and the assertion was the false one.
#[test]
fn a_slot_without_a_credential_is_not_reported_as_active() {
    let t = fixture();
    let root = t.path();

    // A registered codex slot with everything EXCEPT its credential.
    let dir = root.join(".local/share/swapdex/slots/codexish");
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::write(dir.join("config.toml"), b"# nothing\n").unwrap();
    let reg = root.join(".local/share/swapdex/slots.json");
    std::fs::write(
        &reg,
        serde_json::to_vec_pretty(&serde_json::json!([{
            "name": "codexish", "id": "codexish",
            "config_dir": dir.to_string_lossy(), "adopted": false, "tool": "codex"}]))
        .unwrap(),
    )
    .unwrap();

    let (listing, _, _) = run(root, &["ls"]);
    let row = listing
        .lines()
        .find(|l| l.contains("codexish"))
        .unwrap_or_default();
    let (_, err, code) = run(root, &["serve", "codexish", "--tool", "codex"]);

    // Whatever they say, they must not contradict each other: a row cannot be
    // starred as the live account for a tool that refuses to serve it.
    if code == 5 || err.contains("no codex login") {
        assert!(
            !row.contains("codex*"),
            "listing stars a tool whose login serve says is missing:\n{row}\n{err}"
        );
    }
}

/// A registered CODEX slot: the shape that can actually pay for Codex turns.
fn seed_codex_slot(root: &Path, name: &str, email: &str) {
    let dir = root.join(".local/share/swapdex/slots").join(name);
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    // A minimal id_token whose payload carries the email, the way Codex stores it.
    let payload = serde_json::json!({"email": email});
    let b64 = |b: &[u8]| {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut o = String::new();
        for c in b.chunks(3) {
            let n = ((c[0] as u32) << 16)
                | ((*c.get(1).unwrap_or(&0) as u32) << 8)
                | (*c.get(2).unwrap_or(&0) as u32);
            for i in 0..(c.len() + 1) {
                o.push(T[((n >> (18 - i * 6)) & 63) as usize] as char);
            }
        }
        o
    };
    let tok = format!(
        "h.{}.s",
        b64(serde_json::to_string(&payload).unwrap().as_bytes())
    );
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "last_refresh": "2026-08-20T00:00:00Z",
            "tokens": {"id_token": tok, "access_token": "AT", "refresh_token": "RT",
                       "account_id": "acct-1"}}))
        .unwrap(),
    )
    .unwrap();
    chmod600(&dir.join("auth.json"));

    let reg = root.join(".local/share/swapdex/slots.json");
    let mut rows: Vec<serde_json::Value> = std::fs::read(&reg)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    rows.push(serde_json::json!({
        "name": name, "id": name, "config_dir": dir.to_string_lossy(),
        "adopted": false, "tool": "codex"}));
    std::fs::write(&reg, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}

/// Codex accounts must switch the same way Claude ones do.
///
/// Every screen and command was built and fixed against Claude first; Codex
/// took its own branch in the proxy and its own registry entries, so a defect
/// fixed on one side could easily still be live on the other. This walks the
/// same sequence for Codex.
#[test]
fn a_codex_switch_is_visible_and_agrees_across_commands() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cx-one", "one@example.com");
    seed_codex_slot(root, "cx-two", "two@example.com");

    for name in ["cx-one", "cx-two", "cx-one"] {
        let (_, err, code) = run(root, &["serve", name, "--tool", "codex"]);
        assert_eq!(code, 0, "serve {name} --tool codex failed: {err}");

        // The listing must mark exactly this one as paying.
        let (listing, _, _) = run(root, &["ls"]);
        let marked: Vec<&str> = listing.lines().filter(|l| l.contains("pays")).collect();
        assert_eq!(
            marked.len(),
            1,
            "after serving {name} exactly one row should pay:\n{listing}"
        );
        assert!(
            marked[0].contains(name),
            "the paying row names someone else after serving {name}:\n{}",
            marked[0]
        );

        // And the status-bar source must agree.
        let (quiet, _, _) = run(root, &["serve", "--quiet", "--tool", "codex"]);
        assert!(
            quiet.starts_with(name),
            "status bar says '{}' after serving {name}",
            quiet.trim()
        );
    }
}

/// A Codex account listed must say whose login it is.
#[test]
fn a_codex_row_names_its_login() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cx-one", "one@example.com");

    let (listing, _, _) = run(root, &["ls"]);
    let row = listing
        .lines()
        .find(|l| l.contains("cx-one"))
        .unwrap_or_default();
    assert!(
        row.contains("one@example.com"),
        "the codex row does not name its login:\n{row}"
    );
}

/// Renaming a Codex account must move it everywhere too.
#[test]
fn renaming_a_codex_account_moves_it_everywhere() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cx-before", "one@example.com");

    let (_, err, code) = run(root, &["rename", "cx-before", "cx-after"]);
    assert_eq!(code, 0, "rename failed: {err}");

    let (listing, _, _) = run(root, &["ls"]);
    assert!(listing.contains("cx-after"), "new name missing:\n{listing}");
    assert!(
        !listing.contains("cx-before"),
        "old name survived:\n{listing}"
    );
    let (_, err, code) = run(root, &["serve", "cx-after", "--tool", "codex"]);
    assert_eq!(code, 0, "serve does not know the new codex name: {err}");
}

/// A rename must move EVERY tool's slot, not the first one it finds.
///
/// `find_any_tool` returns one tool, and rename renamed that tool's registry
/// entry only. An account holding both a Claude and a Codex slot came out split
/// in two: `cxtest [codex*, claude-code]` beside `codex-test [codex]`, one
/// login under two names. 0.88.0 fixed the slot-vs-snapshot half of this; the
/// slot-vs-slot half was still live.
#[test]
fn a_rename_moves_every_tool_slot() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "both", "b@example.com");
    seed_codex_slot(root, "both", "b@example.com");

    let (_, err, code) = run(root, &["rename", "both", "moved"]);
    assert_eq!(code, 0, "rename failed: {err}");

    let (listing, _, _) = run(root, &["ls"]);
    assert!(
        !listing.contains("both"),
        "the old name survived on some tool:\n{listing}"
    );
    // One account, one row - not one row per tool that got left behind.
    let rows = listing.lines().filter(|l| l.contains("moved")).count();
    assert_eq!(rows, 1, "the account came out split in two:\n{listing}");

    // And both tools answer to the new name.
    for tool in ["claude-code", "codex"] {
        let (_, err, code) = run(root, &["serve", "moved", "--tool", tool]);
        assert_eq!(code, 0, "serve --tool {tool} does not know 'moved': {err}");
    }
}

/// `rm --tool` must be able to drop a SLOT, not just a snapshot.
///
/// It looked in the snapshot store only, so a tool that existed as a slot
/// answered "profile 'X' has no codex login" about a slot the listing was
/// showing - and the only way to remove it was editing slots.json by hand.
#[test]
fn dropping_a_tool_removes_its_slot_too() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "both", "b@example.com");
    seed_codex_slot(root, "both", "b@example.com");

    let (out, err, code) = run(root, &["rm", "both", "--tool", "codex", "--yes"]);
    assert_eq!(code, 0, "dropping the codex slot failed: {err}{out}");

    // The claude side survives...
    let (listing, _, _) = run(root, &["ls"]);
    assert!(listing.contains("both"), "the account vanished:\n{listing}");
    assert!(
        listing.contains("claude-code"),
        "the claude slot was taken too:\n{listing}"
    );
    // ...and the codex side is gone.
    let (_, err, code) = run(root, &["serve", "both", "--tool", "codex"]);
    assert_ne!(code, 0, "codex still serves after being dropped: {err}");
}

/// `ls` must say what the TUI says: an expired slot cannot serve.
///
/// The TUI marked bsgong "expired" and the proxy log agreed - "its login has
/// expired - passing your own login through" - while `ls` showed the row with
/// no note at all. A list that presents an unusable account as fine is the
/// worse of the two, because `ls` is what a script and a glance both read.
#[test]
fn ls_marks_a_slot_whose_token_has_expired() {
    let t = fixture();
    let root = t.path();
    // A slot whose access token lapsed weeks ago.
    let dir = root.join(".local/share/swapdex/slots/old");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "accessToken":"AT","refreshToken":"RT",
            "expiresAt": 1_600_000_000_000i64, "subscriptionType":"max"}}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        br#"{"oauthAccount":{"emailAddress":"old@example.com"}}"#,
    )
    .unwrap();
    chmod600(&dir.join(".credentials.json"));
    std::fs::write(
        root.join(".local/share/swapdex/slots.json"),
        serde_json::to_vec_pretty(&serde_json::json!([{
            "name": "old", "id": "old", "config_dir": dir.to_string_lossy(),
            "adopted": false, "tool": "claude-code"}]))
        .unwrap(),
    )
    .unwrap();

    let (listing, _, _) = run(root, &["ls"]);
    let row = listing
        .lines()
        .find(|l| l.contains("old"))
        .unwrap_or_default();
    assert!(
        row.contains("expired") || row.contains("stale") || row.contains("no login"),
        "an expired slot is listed as if it were fine:\n{row}"
    );
}

/// `ls` must show an account no matter which tool its slot belongs to.
///
/// The listing walked a hardcoded pair of tools while four adapters exist, so
/// an account living only as a Gemini or Antigravity slot had no row at all -
/// switching to it worked and the listing showed nothing, the same "the switch
/// did nothing" appearance already fixed once for Claude and once for Codex.
#[test]
fn ls_lists_an_account_that_lives_only_as_a_gemini_slot() {
    let td = fixture();
    let root = td.path();
    seed_gemini_slot(root, "kong", "kong@example.com");

    let (out, err, code) = run(root, &["ls"]);
    assert_eq!(code, 0, "ls failed: {err}");
    assert!(
        out.contains("kong"),
        "a Gemini-only account is missing from the listing:\n{out}"
    );
}

fn seed_gemini_slot(root: &Path, name: &str, email: &str) {
    let dir = root.join(".local/share/swapdex/slots").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("oauth_creds.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token": "AT", "refresh_token": "RT",
            "expiry_date": 9999999999999i64}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("google_accounts.json"),
        serde_json::to_vec(&serde_json::json!({"active": email, "old": []})).unwrap(),
    )
    .unwrap();
    chmod600(&dir.join("oauth_creds.json"));

    let reg = root.join(".local/share/swapdex/slots.json");
    let mut rows: Vec<serde_json::Value> = std::fs::read(&reg)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    rows.push(serde_json::json!({
        "name": name, "id": name, "config_dir": dir.to_string_lossy(),
        "adopted": false, "tool": "gemini"}));
    std::fs::write(&reg, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}

/// The status bar must never go blank about usage without saying so.
///
/// With no reading in the cache the bar printed the account and stopped. The
/// numbers had been there the day before, so their absence read as a broken
/// status bar - and the two ordinary causes, nothing measured yet and a window
/// that passed its reset before a fresh read landed, both looked identical to
/// a failure.
#[test]
fn the_status_bar_says_when_it_has_no_usage_reading() {
    let td = fixture();
    let root = td.path();
    seed_slot(root, "rnd", "rnd@example.com");
    let (_, err, code) = run(root, &["serve", "rnd"]);
    assert_eq!(code, 0, "serve failed: {err}");

    let (out, err, code) = run(root, &["serve", "--quiet"]);
    assert_eq!(code, 0, "serve --quiet failed: {err}");
    assert!(
        out.contains("usage unread"),
        "the bar went silent about usage instead of saying it had no reading:\n{out:?}"
    );
}

/// A damaged registry must never be reported as "no accounts".
///
/// `slots.json` holds every account on the machine, and until 0.103.0 it was
/// written with a plain truncate-then-write - so an interrupted write could
/// leave it unparseable. `Slots::open_for` says "slots.json is corrupt", but
/// eight callers discard that, and `ls` then printed "No accounts saved yet"
/// with setup advice. That tells a user who still has every credential on disk
/// that they never had an account, and invites them to start over on top of it.
#[test]
fn a_damaged_registry_is_not_reported_as_having_no_accounts() {
    let td = fixture();
    let root = td.path();
    let reg = root.join(".local/share/swapdex/slots.json");
    std::fs::create_dir_all(reg.parent().unwrap()).unwrap();
    std::fs::write(&reg, b"[{\"name\":\"rnd\",").unwrap();

    let (out, err, _) = run(root, &["ls"]);
    let all = format!("{out}{err}").to_lowercase();
    assert!(
        !all.contains("no accounts saved yet"),
        "a damaged registry was reported as an empty one:\n{out}{err}"
    );
    assert!(
        all.contains("registry") || all.contains("corrupt") || all.contains("could not be read"),
        "nothing said the registry could not be read:\n{out}{err}"
    );
}
