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

/// The email the LIVE Claude login carries, read from the tool's own file.
///
/// A test about what a switch WROTE has to read the file. The mark in `ls`
/// follows the slot pointer, which answers who pays - a different question, and
/// under the slot model the two are routinely different accounts.
fn live_claude_email(root: &Path) -> String {
    let bytes = std::fs::read(root.join(".claude.json")).expect("read the live .claude.json");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("parse the live .claude.json");
    v["oauthAccount"]["emailAddress"]
        .as_str()
        .unwrap_or_default()
        .to_string()
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

/// A minimal id_token whose payload carries the email, the way Codex stores it.
fn codex_id_token(email: &str) -> String {
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
    format!(
        "h.{}.s",
        b64(serde_json::to_string(&payload).unwrap().as_bytes())
    )
}

/// A live Codex login in the tool's own dir - what `add` captures from.
fn seed_live_codex(root: &Path, email: &str) {
    let dir = root.join(".codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {"id_token": codex_id_token(email), "access_token": "AT",
                       "refresh_token": "RT", "account_id": "acct-live"}}))
        .unwrap(),
    )
    .unwrap();
    chmod600(&dir.join("auth.json"));
}

/// A registered CODEX slot: the shape that can actually pay for Codex turns.
/// `YYYY-MM-DDTHH:MM:SSZ` from a unix timestamp, the way Codex writes it.
/// Civil-from-days, so no date crate is pulled in for two tests.
fn rfc3339_utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = era * 400 + yoe + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn seed_codex_slot(root: &Path, name: &str, email: &str) {
    let dir = root.join(".local/share/swapdex/slots").join(name);
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    let tok = codex_id_token(email);
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

/// An account for a tool this swapdex does not know must not be invented.
///
/// The tool string went straight to `Slots::open_for`, which does not validate
/// it, so a manifest naming a tool this build has never heard of produced a slot
/// that can never serve - announced as "(claude)", because the display falls
/// back to Claude for an unknown name. swapdex ships four adapters and is built
/// to grow: exporting a Gemini account and importing it on a build that predates
/// Gemini turns it into a Claude account, and `FORMAT_VERSION` cannot catch that
/// because adding an adapter is not a format change.
#[test]
fn importing_an_account_for_an_unknown_tool_refuses_instead_of_guessing() {
    let td = fixture();
    let root = td.path();
    let manifest = root.join("in.json");
    std::fs::write(
        &manifest,
        br#"{"version":1,"accounts":[{"name":"zed","tool":"nosuchtool"},
             {"name":"ok","tool":"claude-code"}]}"#,
    )
    .unwrap();

    let (out, err, _) = run(root, &["import", manifest.to_str().unwrap()]);
    let all = format!("{out}{err}");
    assert!(
        all.contains("nosuchtool"),
        "the unknown tool was never named:\n{all}"
    );
    assert!(
        !all.contains("created zed"),
        "an account was created for a tool this build cannot serve:\n{all}"
    );
    // The rest of the file still imports.
    assert!(
        all.contains("created ok"),
        "a known account was skipped:\n{all}"
    );
}

/// Every command swapdex prints as a remedy must be a command swapdex accepts.
///
/// The remedy text is a second copy of the CLI, kept by hand, and it drifted:
/// `doctor` answered "the proxy service is installed but not running" - the
/// exact shape of an outage - with `swapdex service restart`, which has never
/// existed, and a credential warning pointed at `swapdex whoami`. Reading the
/// source for backtick-quoted commands and asking the binary about each one
/// keeps the two copies honest, and costs nothing to extend: a command that
/// takes subcommands has its second word checked too.
#[test]
fn every_command_we_tell_the_user_to_run_exists() {
    fn words(line: &str) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        let mut rest = line;
        while let Some(i) = rest.find("`swapdex ") {
            rest = &rest[i + "`swapdex ".len()..];
            let mut it = rest.split_whitespace();
            // The leading run of command characters: the token carries the
            // closing backtick, a `<placeholder>` or a `--flag` after it.
            let word = |w: &str| {
                if w.starts_with('-') {
                    return None;
                }
                let t: String = w
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect();
                (!t.is_empty()).then_some(t)
            };
            let Some(first) = it.next().and_then(word) else {
                continue;
            };
            out.push((first, it.next().and_then(word)));
        }
        out
    }

    fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    let mut files = Vec::new();
    rs_files(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
        &mut files,
    );
    assert!(!files.is_empty(), "no source to read");

    let help = |args: &[&str]| {
        Command::new(bin())
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    };

    let mut missing: Vec<String> = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (cmd, sub) in words(&text) {
            if !help(&["help", &cmd]) {
                missing.push(format!("{}: swapdex {cmd}", f.display()));
                continue;
            }
            // Only a command that HAS subcommands can have a wrong one.
            let listing = Command::new(bin()).args(["help", &cmd]).output().unwrap();
            let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
            let Some(sub) = sub.filter(|_| listing.contains("Commands:")) else {
                continue;
            };
            if !help(&[&cmd, &sub, "--help"]) {
                missing.push(format!("{}: swapdex {cmd} {sub}", f.display()));
            }
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "swapdex tells the user to run commands it does not have:\n  {}",
        missing.join("\n  ")
    );
}

/// The status bar must not go on aging a number that has stopped arriving.
///
/// On a real machine the account paying for Codex had its token rejected by
/// the usage endpoint. `quota` said "token rejected"; the bar, which reads only
/// the cache, said "7d 95% . 1h old" and would have said "2h old", "3h old",
/// forever - the two surfaces disagreeing about the same account, with the one
/// people watch giving the reassuring answer.
#[test]
fn the_status_bar_says_a_reading_stopped_arriving_not_that_it_is_late() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");
    run(root, &["serve", "alpha"]);

    let cache = root.join(".local/share/swapdex/quota-cache.json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // A reading taken well over an hour ago, and nothing since.
    std::fs::write(
        &cache,
        serde_json::to_vec(&serde_json::json!({
            "alpha": {"seven_d": 5.0, "at": now - 6300}
        }))
        .unwrap(),
    )
    .unwrap();
    let (late, _, _) = run(root, &["serve", "--quiet"]);
    assert!(
        late.contains("7d 95%") && late.contains("old"),
        "a merely late reading still shows its number and its age: {late}"
    );

    // The same cache, plus the fact that the last read was refused outright.
    std::fs::write(
        &cache,
        serde_json::to_vec(&serde_json::json!({
            "alpha": {"seven_d": 5.0, "at": now - 6300, "token_rejected_at": now - 60}
        }))
        .unwrap(),
    )
    .unwrap();
    let (rejected, _, _) = run(root, &["serve", "--quiet"]);
    assert!(
        rejected.contains("token rejected"),
        "the bar must say why the number stopped: {rejected}"
    );
    assert!(
        !rejected.contains("95%"),
        "and must not keep offering a number nothing can refresh: {rejected}"
    );
}

/// Run `proxy --tool <tool>` and require it to EXIT. Returns (stderr, code).
/// Fails the test if it is still running after a few seconds, which is what
/// "it started a server instead of refusing" looks like from out here.
fn refuse_or_die(root: &Path, tool: &str) -> (String, i32) {
    use std::process::Stdio;
    let mut child = Command::new(bin())
        .args(["proxy", "--tool", tool, "--port", "39999"])
        .env("SWAPDEX_ROOT", root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let out = child.wait_with_output().unwrap();
            return (
                String::from_utf8_lossy(&out.stderr).into_owned(),
                status.code().unwrap_or(-1),
            );
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "`proxy --tool {tool}` is still running - it started a proxy instead of refusing"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A tool the proxy cannot carry must not be offered a proxy or a service.
///
/// `--tool` was accepted for gemini and antigravity and fell through to the
/// Anthropic branch: `swapdex proxy --tool gemini` announced itself as "swapdex
/// claude proxy" and told the reader to point CLAUDE at it, and `service
/// install --tool gemini` wrote a unit running exactly that on Gemini's port,
/// restarted forever by KeepAlive. The command surface said a Gemini proxy
/// existed; the relay only ever spoke to Anthropic and ChatGPT.
#[test]
fn the_proxy_refuses_a_tool_it_has_no_relay_for() {
    let t = fixture();
    let root = t.path();
    for tool in ["gemini", "antigravity"] {
        // Bounded on purpose. When this regressed, the proxy STARTED and served
        // forever, so a blocking `output()` hung instead of failing - a test
        // that hangs on the defect it exists to catch reports nothing at all.
        let (err, code) = refuse_or_die(root, tool);
        assert_ne!(code, 0, "`proxy --tool {tool}` must not start: {err}");
        assert!(
            err.contains(&format!("no {tool} relay")),
            "and must say why, naming the tool: {err}"
        );
        assert!(
            !err.contains("ANTHROPIC_BASE_URL"),
            "it must never hand out Claude's variable for another tool: {err}"
        );

        let (out, err, code) = run(root, &["service", "install", "--tool", tool]);
        assert_ne!(
            code, 0,
            "`service install --tool {tool}` must refuse: {out}{err}"
        );
        let unit = root.join(format!(".config/systemd/user/swapdex-{tool}.service"));
        let plist = root.join(format!(
            "Library/LaunchAgents/io.github.youdie006.swapdex.{tool}.plist"
        ));
        assert!(
            !unit.exists() && !plist.exists(),
            "and must leave no unit behind"
        );
    }

    // The two it DOES carry still get a unit written. (Whether the proxy then
    // comes UP is a different question, and in a fixture with no accounts it
    // does not - which is why this looks at the unit and not the exit code.)
    for (tool, unit) in [("claude", "swapdex-claude"), ("codex", "swapdex-codex")] {
        run(root, &["service", "install", "--tool", tool]);
        let systemd = root.join(format!(".config/systemd/user/{unit}.service"));
        let launchd = root.join(format!(
            "Library/LaunchAgents/io.github.youdie006.swapdex.{tool}.plist"
        ));
        assert!(
            systemd.exists() || launchd.exists(),
            "`service install --tool {tool}` must still write a unit"
        );
    }
}

/// No message swapdex prints carries a run of collapsed whitespace.
///
/// A string literal split across source lines needs a trailing `\`; without it
/// the indentation of the next line lands in the middle of the sentence. Three
/// apply's rollback must read the item it will write back to.
///
/// The journal records `kc_service: effective_computed_service()` and the
/// rollback calls `keychain_write`, which targets that same env-derived item -
/// but the PRIOR value was read through `keychain_service()`, whose fallback
/// returns a lone discovered item when the derived one does not exist. So on a
/// machine with one other profile's Keychain item and no item of its own - what
/// happens right after a sign-out - a failed apply could roll back by writing
/// ANOTHER ACCOUNT'S token into this environment's item. The `None` arm next to
/// it already names that hazard: "a token/identity mismatch a later `use` would
/// silently apply."
///
/// Only macOS has the Keychain, so the pairing is pinned where it is decided.
#[test]
fn the_rollback_reads_the_same_keychain_item_it_writes_back_to() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/claude.rs"),
    )
    .unwrap();
    let at = src
        .find("fn keychain_prior()")
        .expect("the rollback's prior read");
    let end = src[at..].find("\n}\n").expect("the end of that function") + at;
    // Comments explain the hazard by name, so judge the CODE.
    let body: String = src[at..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.as_str();
    assert!(
        body.contains("effective_computed_service()"),
        "prior must read the env-derived item, the one the rollback writes: {body}"
    );
    assert!(
        !body.contains("keychain_service()"),
        "not through the fallback that can return another profile's item: {body}"
    );
}

/// The check that a sign-out took must ask about the item the sign-out deleted.
///
/// `keychain_delete` targets the ENV-DERIVED Keychain item only - "never a
/// discovered one", because removing another `CLAUDE_CONFIG_DIR` profile's login
/// while adding an account would be a disaster. `present` reads through
/// `pick_service`, which falls back to a lone discovered item when the derived
/// one is gone - correct for an alias-only setup, and exactly wrong here: right
/// after the delete the derived item IS gone, the leftover answers instead, and
/// the sign-out reports failure. On a machine with one leftover - what `doctor`
/// calls "1 other Claude item(s)" - no second account could ever be added.
///
/// Only macOS has the Keychain, so no runnable test can reach this on Linux CI.
/// The wiring is what breaks, so the wiring is what is pinned.
#[test]
fn the_sign_out_check_asks_the_narrow_question() {
    let src =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands.rs"))
            .unwrap();
    let at = src
        .find("sign_out_locally(paths, tool);")
        .expect("the local sign-out");
    let after = &src[at..];
    let check = after
        .find("if still_same ||")
        .expect("the verification that it took");
    let line: String = after[check..].lines().next().unwrap().to_string();
    assert!(
        line.contains("managed_present"),
        "the check must ask about the item the delete targeted: {line}"
    );
    assert!(
        !line.contains(".present("),
        "not whether ANY credential can be read: {line}"
    );
}

/// had it, and two of those print on the proxy's request path - so an account
/// with a lapsed subscription was told about it in a sentence with an
/// eighteen-space hole in it, at the moment the tool most needs to be believed.
#[test]
fn no_printed_message_has_a_hole_in_the_middle_of_it() {
    fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs")
                // The wordmark is ASCII art; its spaces ARE the picture.
                && p.file_name().is_some_and(|n| n != "banner.rs")
            {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    rs_files(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
        &mut files,
    );

    let mut bad: Vec<String> = Vec::new();
    for f in &files {
        for (i, line) in std::fs::read_to_string(f).unwrap().lines().enumerate() {
            let t = line.trim_start();
            // Comments and doc comments wrap freely; this is about literals.
            if t.starts_with("//") || !line.contains('"') {
                continue;
            }
            // A width/alignment spec marks a table row, where runs of spaces
            // are the columns. Plain `{}` interpolation is not one.
            if line.contains("{:") {
                continue;
            }
            // The shape that is always wrong: mid-SENTENCE. A letter or a
            // sentence mark, then a run long enough to be source indentation,
            // then more text on the same line. Column alignment does not look
            // like this - it follows a `:` or a `}` format spec - and a
            // deliberate output indent follows an escaped newline.
            let b = line.as_bytes();
            for w in 1..b.len().saturating_sub(7) {
                let prev = b[w];
                let sentence = prev.is_ascii_alphabetic() || matches!(prev, b'.' | b',' | b')');
                // `\n` inside a literal: the `n` is a letter but not a word.
                if !sentence || b[w - 1] == b'\\' {
                    continue;
                }
                let run = b[w + 1..].iter().take_while(|&&c| c == b' ').count();
                if run >= 6 && b.get(w + 1 + run).is_some_and(|&c| c != b'/') {
                    bad.push(format!("{}:{}: {}", f.display(), i + 1, line.trim()));
                    break;
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a printed string carries source indentation inside it:\n  {}",
        bad.join("\n  ")
    );
}

/// Every command that REGISTERS an account enforces the same name rule.
///
/// `add`, `use`, `rm`, `rename` and `login` all called it. `run` and `adopt` -
/// the two that actually create a slot - did not, so `adopt '../evil' <dir>`
/// registered that name and `run` with a carriage return in the name made a
/// permanent account that `ls` renders as something other than what it is.
#[test]
fn a_name_no_command_would_accept_cannot_be_registered_by_another() {
    let t = fixture();
    let root = t.path();
    let dir = root.join("adopted");
    std::fs::create_dir_all(&dir).unwrap();

    for bad in ["../evil", "a/b", ".hidden", "good\rEVIL"] {
        let (out, err, code) = run(root, &["adopt", bad, dir.to_str().unwrap()]);
        assert_ne!(code, 0, "adopt accepted {bad:?}: {out}{err}");
        let (out, err, code) = run(root, &["run", bad, "--no-launch"]);
        assert_ne!(code, 0, "run accepted {bad:?}: {out}{err}");
    }

    // Nothing was registered, and the refusal never echoes a control character.
    let listing = std::fs::read_to_string(root.join(".local/share/swapdex/slots.json"))
        .unwrap_or_else(|_| "[]".into());
    assert!(
        !listing.contains("evil"),
        "a refused name was registered: {listing}"
    );
    assert!(
        !listing.contains("EVIL"),
        "a refused name was registered: {listing}"
    );
    let (_, err, _) = run(root, &["adopt", "good\rEVIL", dir.to_str().unwrap()]);
    assert!(
        !err.contains('\r'),
        "the refusal echoed the control character: {err:?}"
    );
}

/// A name reserved where accounts are CREATED is reserved for every creator.
///
/// `add` refuses `-` because `swapdex use -` toggles to the previous account.
/// `adopt` and `run` register accounts too, and they check only the permanent
/// rule, which deliberately ALLOWS `-` so a legacy account carrying that name
/// stays rm-able. So `adopt - <dir>` registered it, and `use -` then selected
/// that account instead of toggling - the harm the reservation exists to stop.
#[test]
fn a_name_reserved_at_creation_is_refused_by_every_creator() {
    let t = fixture();
    let root = t.path();
    let dir = root.join("adopted");
    std::fs::create_dir_all(&dir).unwrap();

    let (out, err, code) = run(root, &["adopt", "-", dir.to_str().unwrap()]);
    assert_ne!(code, 0, "adopt accepted '-': {out}{err}");
    let (out, err, code) = run(root, &["run", "-", "--no-launch"]);
    assert_ne!(code, 0, "run accepted '-': {out}{err}");

    // The whole leading-dash range, not just the toggle. clap refuses these as
    // bare positionals, so they arrive the only way they can - after `--`, the
    // form migrate and a 0.2.x store can still hand over.
    let (out, err, code) = run(root, &["adopt", "--", "-x", dir.to_str().unwrap()]);
    assert_ne!(code, 0, "adopt accepted '-x': {out}{err}");
    let (out, err, code) = run(root, &["run", "--no-launch", "--", "-x"]);
    assert_ne!(code, 0, "run accepted '-x': {out}{err}");
    let (out, err, code) = run(root, &["add", "--", "-x"]);
    assert_ne!(code, 0, "add accepted '-x': {out}{err}");

    let rows: Vec<serde_json::Value> = std::fs::read(root.join(".local/share/swapdex/slots.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for r in &rows {
        let n = r["name"].as_str().unwrap_or_default();
        assert!(
            !n.starts_with('-'),
            "a name reserved at creation was registered: {rows:?}"
        );
    }
}

/// The reservation is at creation only: an account already named `-` stays
/// manageable.
///
/// That is why the permanent rule allows the name, and why the refusal above
/// must not move into it - a 0.2.x account named `-` would otherwise be stuck,
/// unable to be renamed out of the name or removed.
#[test]
fn an_account_already_named_dash_can_still_be_renamed_and_removed() {
    let t = fixture();
    let root = t.path();

    seed_slot(root, "-", "dash@example.com");
    let (out, err, code) = run(root, &["rename", "-", "dash"]);
    assert_eq!(code, 0, "a legacy '-' could not be renamed: {out}{err}");

    seed_slot(root, "-", "dash2@example.com");
    let (out, err, code) = run(root, &["rm", "-", "--yes"]);
    assert_eq!(code, 0, "a legacy '-' could not be removed: {out}{err}");
}

/// An account's rotation preferences must answer to the same name the account does.
///
/// `settings.json` keys `disabled` and `priority` by account name, and the proxy
/// filters on exact string equality. `rename` moved the slot registries and the
/// stored profile but left the preferences pointing at a name that no longer
/// exists, so a paused account quietly rejoined rotation - the one thing pausing
/// it was meant to prevent. `rm` left the entry behind entirely, so a later
/// account that happened to reuse the name was born paused and pre-ranked, with
/// nothing on any screen to say why.
#[test]
fn rotation_preferences_follow_a_rename_and_leave_with_a_removal() {
    let t = fixture();
    let root = t.path();
    let settings = root.join(".local/share/swapdex/settings.json");

    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");

    std::fs::write(
        &settings,
        br#"{"disabled":["alpha"],"priority":["other","alpha"]}"#,
    )
    .unwrap();

    let (_, err, code) = run(root, &["rename", "alpha", "alpha2"]);
    assert_eq!(code, 0, "rename failed: {err}");
    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(
        !after.contains("\"alpha\""),
        "the old name still holds the preferences after a rename: {after}"
    );
    assert!(
        after.contains("alpha2"),
        "the pause did not follow the account: {after}"
    );

    // This name is both a slot and a saved profile, so the first `rm` retires
    // only the slot - the profile still answers to the name, and so do its
    // preferences.
    let (out, err, code) = run(root, &["rm", "alpha2", "--yes"]);
    assert_eq!(code, 0, "rm failed: {err}");
    assert!(
        out.contains("still here"),
        "expected a partial removal: {out}"
    );
    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(
        after.contains("alpha2"),
        "a partial removal dropped preferences the profile still owns: {after}"
    );

    // The second retires the profile, and now nothing answers to the name.
    let (_, err, code) = run(root, &["rm", "alpha2", "--yes"]);
    assert_eq!(code, 0, "rm failed: {err}");
    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(
        !after.contains("alpha2"),
        "a removed account left its preferences for the next account of that name: {after}"
    );
}

/// A remembered usage reading must answer to the same name the account does.
///
/// `quota-cache.json` keys the last-known windows by account name and every
/// display looks them up by exact name. `rename` moved the account and left the
/// reading behind, so the old name went on drawing a row - a phantom account
/// with numbers - while the renamed one showed as never read. `rm` left it
/// behind entirely, so the next account registered under that name inherited a
/// stranger's percentages, its credit status and its sign-out.
///
/// The cache is sharded per tool, so this has to walk EVERY tool's file - the
/// same lesson `rename` already learned about the slot registries.
#[test]
fn remembered_usage_follows_a_rename_and_leaves_with_a_removal() {
    let t = fixture();
    let root = t.path();
    let store = root.join(".local/share/swapdex");
    let claude_cache = store.join("quota-cache.json");
    let codex_cache = store.join("codex-quota-cache.json");

    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "alpha", "a@example.com");

    let reading = |pct: f64| {
        serde_json::to_vec(&serde_json::json!({"alpha": {
            "five_h": pct, "five_h_reset": 9_000_000_000i64,
            "seven_d": pct, "seven_d_reset": 9_000_000_000i64, "at": 1_700_000_000i64}}))
        .unwrap()
    };
    std::fs::write(&claude_cache, reading(22.0)).unwrap();
    std::fs::write(&codex_cache, reading(44.0)).unwrap();

    let (_, err, code) = run(root, &["rename", "alpha", "alpha2"]);
    assert_eq!(code, 0, "rename failed: {err}");
    for f in [&claude_cache, &codex_cache] {
        let after = std::fs::read_to_string(f).unwrap();
        assert!(
            !after.contains("\"alpha\""),
            "{}: the old name still holds the reading: {after}",
            f.display()
        );
        assert!(
            after.contains("alpha2"),
            "{}: the reading did not follow the account: {after}",
            f.display()
        );
    }

    // `alpha2` is both a slot and a saved profile, so the first `rm` retires
    // only the slot - the profile still answers to the name, and so does its
    // remembered usage.
    let (out, err, code) = run(root, &["rm", "alpha2", "--yes"]);
    assert_eq!(code, 0, "rm failed: {err}");
    assert!(
        out.contains("still here"),
        "expected a partial removal: {out}"
    );
    assert!(
        std::fs::read_to_string(&claude_cache)
            .unwrap()
            .contains("alpha2"),
        "a partial removal dropped a reading the profile still owns"
    );

    // The second retires the profile, and now nothing answers to the name.
    let (_, err, code) = run(root, &["rm", "alpha2", "--yes"]);
    assert_eq!(code, 0, "rm failed: {err}");
    for f in [&claude_cache, &codex_cache] {
        let after = std::fs::read_to_string(f).unwrap();
        assert!(
            !after.contains("alpha2"),
            "{}: a removed account left its numbers for the next account of that name: {after}",
            f.display()
        );
    }
}

/// Dropping ONE tool from an account is a partial removal: the account is still
/// here, so everything keyed by its name stays with it.
///
/// `rm --tool` unregisters one slot and leaves the rest of the account alone,
/// and both name-keyed stores - its rotation preferences and its remembered
/// usage - still have an owner. Forgetting them here would silently un-pause an
/// account that is still listed and blank a reading it still answers to, from a
/// command that never claimed to remove the account at all.
#[test]
fn dropping_one_tool_keeps_the_side_state_the_account_still_owns() {
    let t = fixture();
    let root = t.path();
    let store = root.join(".local/share/swapdex");
    let settings = store.join("settings.json");
    let claude_cache = store.join("quota-cache.json");

    seed_slot(root, "both", "b@example.com");
    seed_codex_slot(root, "both", "b@example.com");
    std::fs::write(
        &settings,
        br#"{"disabled":["both"],"priority":["other","both"]}"#,
    )
    .unwrap();
    std::fs::write(
        &claude_cache,
        serde_json::to_vec(&serde_json::json!({"both": {
            "five_h": 22.0, "five_h_reset": 9_000_000_000i64,
            "seven_d": 22.0, "seven_d_reset": 9_000_000_000i64, "at": 1_700_000_000i64}}))
        .unwrap(),
    )
    .unwrap();

    let (out, err, code) = run(root, &["rm", "both", "--tool", "codex", "--yes"]);
    assert_eq!(code, 0, "dropping the codex slot failed: {err}{out}");
    let (listing, _, _) = run(root, &["ls"]);
    assert!(listing.contains("both"), "the account vanished:\n{listing}");

    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(
        after.contains("both"),
        "dropping one tool un-paused an account that is still listed: {after}"
    );
    let after = std::fs::read_to_string(&claude_cache).unwrap();
    assert!(
        after.contains("both"),
        "dropping one tool blanked a reading the account still owns: {after}"
    );
}

/// A bare `rm <name>` must remove the account under EVERY tool, not the first
/// registry that happens to hold the name.
///
/// One name is one account across tools - `ls` draws a two-tool account as a
/// single row, and the prompt asks to "stop managing account '<name>'". The
/// removal opened one registry, so `rm both --yes` printed "stopped managing
/// 'both'." and left `both [codex]` listed, silently: the profile case prints
/// a "run it again" note, the second-tool case printed nothing. It also
/// dropped the account's preferences, readings and ledger row on the way out,
/// because the code below it assumed the account was now gone.
///
/// `mv` already loops every registry for this exact split - "an account holding
/// both a Claude and a Codex slot came out split in two - one login under two
/// names".
#[test]
fn removing_an_account_removes_it_under_every_tool() {
    let t = fixture();
    let root = t.path();
    let store = root.join(".local/share/swapdex");
    let settings = store.join("settings.json");

    seed_slot(root, "both", "b@example.com");
    seed_codex_slot(root, "both", "b@example.com");
    std::fs::write(
        &settings,
        br#"{"disabled":["both"],"priority":["other","both"]}"#,
    )
    .unwrap();

    let (out, err, code) = run(root, &["rm", "both", "--yes"]);
    assert_eq!(code, 0, "removing the account failed: {err}{out}");

    let (listing, _, _) = run(root, &["ls"]);
    assert!(
        !listing.contains("both"),
        "one `rm` left half the account listed under the other tool:\n{listing}"
    );
    let (slots, _, _) = run(root, &["slots"]);
    assert!(
        !slots.contains("both"),
        "the slot survived under a tool `rm` never opened:\n{slots}"
    );

    // Nothing answers to the name now, so its side state goes with it.
    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(
        !after.contains("both"),
        "a fully removed account kept its rotation preferences: {after}"
    );
}

/// The ledger that attributes usage must answer to the same name the account
/// does - whether or not the account ever saved a snapshot.
///
/// `timeline.jsonl` credits every session and every token to a name, and
/// `Store::rename` rewrites it for exactly that reason: leaving the old name
/// there makes `usage`/`sessions` report an account that no longer exists,
/// forever. But `rename` reaches that rewrite only when a profile moves. A
/// slot-only account takes the earlier branch, which carries its rotation
/// preferences and its remembered usage and nothing else, so the ledger goes
/// on naming an account the listing no longer knows.
#[test]
fn the_ledger_follows_a_slot_only_rename() {
    let t = fixture();
    let root = t.path();
    let timeline = root.join(".local/share/swapdex/timeline.jsonl");

    seed_claude(root, "uuid-live", "live@example.com");
    seed_slot(root, "before", "b@example.com");

    let (_, err, code) = run(root, &["use", "before"]);
    assert_eq!(code, 0, "switching to the slot failed: {err}");
    let ledger = std::fs::read_to_string(&timeline).unwrap();
    assert!(
        ledger.contains("\"before\""),
        "the switch left no record to carry: {ledger}"
    );

    let (_, err, code) = run(root, &["rename", "before", "after"]);
    assert_eq!(code, 0, "rename failed: {err}");

    let (listing, _, _) = run(root, &["ls"]);
    assert!(
        !listing.contains("before") && listing.contains("after"),
        "the listing did not follow the rename:\n{listing}"
    );
    let ledger = std::fs::read_to_string(&timeline).unwrap();
    assert!(
        !ledger.contains("\"before\""),
        "the ledger still credits a name nothing answers to: {ledger}"
    );
    assert!(
        ledger.contains("\"after\""),
        "the ledger did not follow the account: {ledger}"
    );
}

/// A removed account must not hand its sessions and tokens to the next account
/// that takes its name.
///
/// Both of the other name-keyed stores drop their account on removal and both
/// say why in the source: what stays behind is inherited by whatever is
/// registered under that name next. `timeline.jsonl` is the third store keyed
/// by name, and `rm` leaves it untouched - so a stranger who registers the
/// freed name is credited with a removed account's history.
///
/// Deleting the events is not the fix. `usage` attributes each transcript to
/// the account active at its timestamp, so erasing the removed account's switch
/// hands its tokens to whichever account switched BEFORE it - a different,
/// innocent account. The name has to be retired in place.
#[test]
fn a_removed_account_does_not_hand_its_usage_to_the_next_account_of_that_name() {
    let t = fixture();
    let root = t.path();
    let timeline = root.join(".local/share/swapdex/timeline.jsonl");

    seed_claude(root, "uuid-live", "live@example.com");
    seed_slot(root, "keeper", "keeper@example.com");
    seed_slot(root, "ghost", "ghost@example.com");

    // `keeper` switches first, so erasing `ghost` would move ghost's tokens
    // onto keeper rather than off the books.
    for name in ["keeper", "ghost"] {
        let (_, err, code) = run(root, &["use", name]);
        assert_eq!(code, 0, "switching to {name} failed: {err}");
    }

    // Tokens spent a minute after the switch to `ghost`, so the ledger has
    // something to attribute.
    let ledger = std::fs::read_to_string(&timeline).unwrap();
    let last: serde_json::Value = serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
    let spent_at = last["ts"].as_i64().unwrap() + 60;
    let proj = root.join(".claude/projects/p");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("s.jsonl"),
        serde_json::to_vec(&serde_json::json!({
            "timestamp": swapdex::refresh::rfc3339_utc(spent_at),
            "message": {"usage": {"input_tokens": 12000, "output_tokens": 3000}}}))
        .unwrap(),
    )
    .unwrap();

    let (out, _, _) = run(root, &["usage"]);
    assert!(
        out.contains("@ghost"),
        "the switch did not attribute the tokens to ghost:\n{out}"
    );

    let (_, err, code) = run(root, &["rm", "ghost", "--yes"]);
    assert_eq!(code, 0, "rm failed: {err}");

    // A different person registers the freed name.
    seed_slot(root, "ghost", "stranger@example.com");

    let (out, _, _) = run(root, &["usage"]);
    assert!(
        !out.contains("@ghost"),
        "a removed account handed its tokens to the next account of that name:\n{out}"
    );
    assert!(
        !out.contains("@keeper"),
        "erasing the removed account moved its tokens onto the account before it:\n{out}"
    );
}

/// One directory is one account, or a pointer cannot name a payer.
///
/// `Slots::payer` resolves a pointer back to a name with `.find` - the pointer
/// files hold PATHS - and its doc says it lives in one place so that what a
/// screen claims and what the proxy does cannot drift apart. That holds only
/// while a directory belongs to one account. `adopt` rejected a duplicate NAME
/// and never a duplicate DIRECTORY, while onboarding - which calls that same
/// `adopt` - filters out the directories already registered before it does. Two
/// records on one directory made `use beta` write "beta" into the ledger while
/// `ls` and `serve --quiet` both credited alpha.
#[test]
fn one_directory_is_one_account() {
    let t = fixture();
    let root = t.path();
    let shared = root.join("home/.claude-shared");
    std::fs::create_dir_all(&shared).unwrap();
    let shared = shared.to_str().unwrap();

    let (out, err, code) = run(root, &["adopt", "alpha", shared]);
    assert_eq!(code, 0, "the first adopt was refused: {out}{err}");

    let (out, err, code) = run(root, &["adopt", "beta", shared]);
    assert_ne!(
        code, 0,
        "a second account was registered on alpha's directory: {out}{err}"
    );
    assert!(
        err.contains("alpha"),
        "the refusal has to name the account already holding it: {err}"
    );

    // Nothing half-written: the refused name reaches no screen, and the account
    // that does hold the directory is still the one every screen credits.
    let (listing, _, _) = run(root, &["ls"]);
    assert!(
        !listing.contains("beta"),
        "a refused adopt still left a row:\n{listing}"
    );
    run(root, &["use", "alpha"]);
    let (quiet, _, _) = run(root, &["serve", "--quiet"]);
    assert!(
        quiet.starts_with("alpha"),
        "the status bar credits '{}' after `use alpha`",
        quiet.trim()
    );
}

/// The refusal is about ambiguity, not about seeing a directory twice.
///
/// A registry is opened FOR one tool and resolves pointers among that tool's
/// records only, and each tool keeps its own pointer files. A directory that is
/// a home for claude and a home for codex therefore names one account on each
/// side, and adopting it for the second tool has to keep working.
#[test]
fn each_tool_may_adopt_the_same_directory() {
    let t = fixture();
    let root = t.path();
    let shared = root.join("home/.shared");
    std::fs::create_dir_all(&shared).unwrap();
    let shared = shared.to_str().unwrap();

    let (out, err, code) = run(root, &["adopt", "alpha", shared]);
    assert_eq!(code, 0, "the claude adopt was refused: {out}{err}");
    let (out, err, code) = run(root, &["adopt", "alpha", shared, "--tool", "codex"]);
    assert_eq!(
        code, 0,
        "codex could not adopt the directory claude holds: {out}{err}"
    );
}

/// `use` has to reach every account the listing shows, whatever tool holds it.
///
/// `use` asked ONE registry - the helper that reads `--tool` collapses "no
/// --tool given" to claude-code - while `ls` asks every registry. A Codex-only
/// account was therefore listed and then rejected: `use cxonly` printed "no
/// profile named 'cxonly'" and exited 5, and only `--tool codex` moved it.
/// `use --help` promises the opposite: "default: every tool the profile has".
#[test]
fn a_codex_only_account_is_reachable_by_a_bare_use() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cxonly", "cx@example.com");

    let (listing, _, _) = run(root, &["ls"]);
    assert!(
        listing.contains("cxonly"),
        "`ls` does not list it:\n{listing}"
    );

    let (_, err, code) = run(root, &["use", "cxonly"]);
    assert_eq!(
        code, 0,
        "`use cxonly` rejected an account `ls` shows: {err}"
    );

    // The default is who pays while nobody is serving, so the listing has to
    // name it - the same check every other switch here makes.
    let (after, _, _) = run(root, &["ls"]);
    let marked: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(
        marked.len(),
        1,
        "after `use cxonly` exactly one row should pay:\n{after}"
    );
    assert!(
        marked[0].contains("cxonly"),
        "the paying row names someone else:\n{}",
        marked[0]
    );
}

/// One row in `ls` is one account, so `use` moves all of it.
///
/// A name can be a saved Claude profile and a Codex slot at once, and `ls`
/// draws that as a single row. `use mix` reported success having moved only
/// the Claude half: the Codex default was left where it was, with nothing on
/// any screen saying so.
#[test]
fn use_moves_every_tool_of_an_account_that_spans_two() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-mix", "mix@example.com");
    run(root, &["add", "mix"]);
    seed_codex_slot(root, "mix", "mix@example.com");
    // Claude is signed in as someone else, so the Claude half is a real move.
    seed_claude(root, "uuid-oth", "oth@example.com");
    run(root, &["add", "other"]);

    let (_, err, code) = run(root, &["use", "mix"]);
    assert_eq!(code, 0, "`use mix` failed: {err}");

    let (after, _, _) = run(root, &["ls"]);
    let row = after
        .lines()
        .find(|l| l.trim_start_matches(['*', ' ']).starts_with("mix "))
        .unwrap_or_else(|| panic!("no row for mix:\n{after}"));
    assert!(
        row.contains("claude-code*"),
        "the claude half did not move:\n{after}"
    );
    assert!(
        row.contains("pays"),
        "the codex half did not move:\n{after}"
    );
}

/// A slot switch must not also copy a credential over the live login.
///
/// `migrate --tool claude` leaves an account holding both a Claude slot and a
/// Claude snapshot, so `use` had two ways to move the same tool and took both:
/// it repointed the default AND wrote the snapshot over whoever was signed in.
/// A credential copy is what the slot model buys its way out of - it is what
/// makes a rotating token log the other session out.
#[test]
fn a_slot_switch_does_not_copy_over_the_live_login() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-dual", "dual@example.com");
    seed_live_codex(root, "dual@example.com");
    run(root, &["add", "dual"]);
    // Claude gets a slot, Codex does not, so the profile still covers a tool no
    // slot took and the copy-model path still has something to do.
    run(root, &["migrate", "--tool", "claude"]);
    // Someone else is signed in to Claude, so a copy would be visible.
    seed_claude(root, "uuid-oth", "oth@example.com");
    run(root, &["add", "other"]);

    let (_, err, code) = run(root, &["use", "dual"]);
    assert_eq!(code, 0, "`use dual` failed: {err}");

    let (after, _, _) = run(root, &["ls"]);
    // The slot did move: the default pays while nobody is serving.
    let marked: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(marked.len(), 1, "exactly one row should pay:\n{after}");
    assert!(
        marked[0].contains("dual"),
        "the paying row names someone else:\n{}",
        marked[0]
    );
    // Nothing was written to the live Claude dir.
    assert_eq!(
        live_claude_email(root),
        "oth@example.com",
        "the live claude login was overwritten:\n{after}"
    );
}

/// `restore` undoes the LAST switch. A slot switch moves a pointer, not a
/// credential, so its undo is repointing the default back - the copy-model
/// backup belongs to some older switch. Reaching for that backup left the slot
/// switch standing and wrote a third account, saved nowhere, over the live
/// login instead.
#[test]
fn restore_undoes_a_slot_switch_not_an_older_backup() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "alpha@example.com");
    run(root, &["add", "alpha"]);
    seed_claude(root, "uuid-b", "beta@example.com");
    run(root, &["add", "beta"]);
    run(root, &["migrate", "--tool", "claude"]);
    // Added after the migration, so it is a profile and no slot: `use` still
    // has a copy-model path to take, and that path leaves a backup.
    seed_claude(root, "uuid-g", "gamma@example.com");
    run(root, &["add", "gamma"]);
    // Signed in by hand and saved nowhere - what the stale backup holds.
    seed_claude(root, "uuid-d", "delta@example.com");

    run(root, &["use", "gamma"]);
    run(root, &["use", "beta"]);
    run(root, &["use", "alpha"]);

    let (_, err, code) = run(root, &["restore"]);
    assert_eq!(code, 0, "`restore` failed: {err}");

    let (after, _, _) = run(root, &["ls"]);
    let marked: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(marked.len(), 1, "exactly one row should pay:\n{after}");
    assert!(
        marked[0].contains("beta"),
        "the slot switch was not undone:\n{after}"
    );
    assert_eq!(
        live_claude_email(root),
        "gamma@example.com",
        "the live login was overwritten from an older backup:\n{after}"
    );
}

/// The other side of the same seam: in a store that holds slots, a switch that
/// went the copy model still has to come back from the credential backup.
/// Repointing a default there would leave the live login where the bad switch
/// put it.
#[test]
fn restore_of_a_copy_model_switch_still_uses_the_backup() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "alpha@example.com");
    run(root, &["add", "alpha"]);
    seed_claude(root, "uuid-b", "beta@example.com");
    run(root, &["add", "beta"]);
    run(root, &["migrate", "--tool", "claude"]);
    seed_claude(root, "uuid-g", "gamma@example.com");
    run(root, &["add", "gamma"]);
    seed_claude(root, "uuid-d", "delta@example.com");

    let (before, _, _) = run(root, &["ls"]);
    let payer_before: Vec<&str> = before.lines().filter(|l| l.contains("pays")).collect();

    run(root, &["use", "gamma"]);
    let (out, err, code) = run(root, &["restore"]);
    assert_eq!(code, 0, "`restore` failed: {err}");
    assert!(
        out.contains("delta@example.com"),
        "the backed-up login did not come back:\n{out}"
    );

    let (after, _, _) = run(root, &["ls"]);
    let payer_after: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(
        payer_before, payer_after,
        "a copy-model restore moved the default pointer:\n{after}"
    );
}

/// Selecting the account that is already the default records a switch that
/// changed nothing. Undoing that one has to reach past it, or `restore` puts
/// back what is already there and the switch before it is stranded.
#[test]
fn restore_reaches_past_a_slot_switch_that_changed_nothing() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "alpha@example.com");
    run(root, &["add", "alpha"]);
    seed_claude(root, "uuid-b", "beta@example.com");
    run(root, &["add", "beta"]);
    run(root, &["migrate", "--tool", "claude"]);

    run(root, &["use", "beta"]);
    run(root, &["use", "alpha"]);
    run(root, &["use", "alpha"]);

    let (_, err, code) = run(root, &["restore"]);
    assert_eq!(code, 0, "`restore` failed: {err}");
    let (after, _, _) = run(root, &["ls"]);
    let marked: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(marked.len(), 1, "exactly one row should pay:\n{after}");
    assert!(
        marked[0].contains("beta"),
        "`restore` stopped at the switch that changed nothing:\n{after}"
    );
}

/// A slot account answers to the prefix its own row is listed under.
///
/// `ls` draws one row per account across every registry and `use <exact-name>`
/// repoints a slot fine, but the name resolver drew its candidates from the
/// snapshot store alone. A slot-only owner - the whole point of the slot model -
/// therefore had the short name they type every day rejected as a profile that
/// does not exist.
#[test]
fn a_slot_only_account_answers_to_its_prefix() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");
    seed_slot(root, "workacct", "workacct@example.com");

    let (listed, _, _) = run(root, &["ls"]);
    assert!(
        listed.contains("personal"),
        "the slot is listed under this name:\n{listed}"
    );

    let (out, err, code) = run(root, &["use", "pers"]);
    assert_eq!(code, 0, "a unique prefix should switch: {err}{out}");
    let (after, _, _) = run(root, &["ls"]);
    let pays: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(pays.len(), 1, "exactly one row should pay:\n{after}");
    assert!(
        pays[0].contains("personal"),
        "the prefix should have switched the account it matched:\n{after}"
    );
}

/// `-` toggles between slot accounts too, and says which ones when it cannot.
///
/// The toggle reads the switch timeline, which a slot switch writes like any
/// other - but it then dropped every candidate the snapshot store did not know.
/// With no store profile at all the remedy it printed named an empty set,
/// `swapdex use <>`, a command nobody can type.
#[test]
fn the_toggle_reaches_a_slot_only_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");
    seed_slot(root, "workacct", "workacct@example.com");

    let (_, err, code) = run(root, &["use", "-"]);
    assert_ne!(
        code, 0,
        "nothing has been switched yet, so '-' means nothing"
    );
    assert!(
        err.contains("personal") && err.contains("workacct"),
        "the remedy has to name the accounts that exist:\n{err}"
    );

    run(root, &["use", "personal"]);
    run(root, &["use", "workacct"]);
    let (out, err, code) = run(root, &["use", "-"]);
    assert_eq!(
        code, 0,
        "'-' should go back to the previous slot: {err}{out}"
    );
    let (after, _, _) = run(root, &["ls"]);
    let pays: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(pays.len(), 1, "exactly one row should pay:\n{after}");
    assert!(
        pays[0].contains("personal"),
        "'-' should have gone back to the account before this one:\n{after}"
    );
}

/// A prefix on an account that owns a slot still moves the pointer.
///
/// `use` asked the slot registries about the raw argument, so a prefix - which
/// no registry has a row for - looked like a legacy copy-model profile and took
/// the copy path. That path writes a credential over the very slot the account
/// already has, which is what the slot model exists to prevent.
#[test]
fn a_prefix_on_a_migrated_account_repoints_its_slot() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "alpha@example.com");
    run(root, &["add", "alpha"]);
    seed_claude(root, "uuid-b", "beta@example.com");
    run(root, &["add", "beta"]);
    run(root, &["migrate", "--tool", "claude"]);

    let (exact, _, _) = run(root, &["use", "alpha"]);
    let (prefix, err, code) = run(root, &["use", "bet"]);
    assert_eq!(code, 0, "a unique prefix should switch: {err}{prefix}");
    assert_eq!(
        prefix.lines().next().map(|l| l.replace("beta", "alpha")),
        exact.lines().next().map(|l| l.to_string()),
        "the prefix took a different path than the name it expands to:\n\
         exact:  {exact}\nprefix: {prefix}"
    );
}

/// A prefix is resolved against the tool the switch is for.
///
/// Name resolution reaches into the slot registries, and has to stay inside the
/// selection `--tool` made. An account only Claude holds a slot for is not a
/// candidate for a Codex switch: counting it makes the Codex account's own
/// prefix read as two accounts, so the short name stops working the moment a
/// neighbour is registered for some other tool.
#[test]
fn a_prefix_resolves_only_against_the_selected_tool() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "shared-claude", "claude@example.com");
    seed_codex_slot(root, "shared-codex", "codex@example.com");

    let (out, err, code) = run(root, &["use", "shared", "--tool", "codex"]);
    assert_eq!(code, 0, "'shared' names one codex account: {err}{out}");
    let (after, _, _) = run(root, &["ls"]);
    assert!(
        after
            .lines()
            .any(|l| l.contains("shared-codex") && l.contains("pays")),
        "the codex account the prefix names should be paying:\n{after}"
    );
}

/// A stub tool binary on PATH, so `--open` has something to exec. Returns the
/// PATH to run with.
fn fake_tool(root: &Path, bin_name: &str, marker: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.join("fakebin");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(bin_name);
    std::fs::write(&p, format!("#!/bin/sh\necho {marker}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// `run`, with the stub tools ahead of everything on PATH.
fn run_on_path(root: &Path, args: &[&str], path: &str) -> (String, String, i32) {
    let out = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("PATH", path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// `--open` switches the same accounts a plain `use` switches.
///
/// A slot-only account is what the slot model produces: registered under a
/// tool, holding no snapshot. `use <name>` repoints its pointer, and `--open`
/// is that same switch plus a launch - but it went straight to the copy-model
/// path, which asks the snapshot store alone. So the account `ls` lists and
/// `use` had just switched to was rejected as a profile that does not exist.
#[test]
fn open_reaches_a_slot_only_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");
    seed_slot(root, "workacct", "workacct@example.com");
    let path = fake_tool(root, "claude", "CLAUDE-OPENED");

    let (out, err, code) = run_on_path(
        root,
        &["use", "workacct", "--tool", "claude", "--open"],
        &path,
    );
    assert_eq!(code, 0, "`use --open` on a listed account: {err}{out}");
    assert!(
        out.contains("CLAUDE-OPENED"),
        "claude should launch after the switch:\n{out}{err}"
    );
    let (after, _, _) = run(root, &["ls"]);
    assert!(
        after
            .lines()
            .any(|l| l.contains("workacct") && l.contains("pays")),
        "`--open` must make the switch `use` makes:\n{after}"
    );
}

/// `--open` must not copy a credential over the slot it just repointed.
///
/// An account that came through `migrate` holds both a slot and a snapshot, so
/// there are two ways to move the same tool. `use` asks the slot registry and
/// repoints the pointer; `--open` never asked it, so the snapshot was written
/// over whoever was signed in - the very copy the slot model exists to avoid -
/// and the pointer the switch was supposed to move stayed where it was.
#[test]
fn open_does_not_copy_over_the_slot_it_switched() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-dual", "dual@example.com");
    run(root, &["add", "dual"]);
    run(root, &["migrate", "--tool", "claude"]);
    // Someone else is signed in to Claude, so a copy would be visible.
    seed_claude(root, "uuid-oth", "oth@example.com");
    run(root, &["add", "other"]);
    let path = fake_tool(root, "claude", "CLAUDE-OPENED");

    let (out, err, code) = run_on_path(root, &["use", "dual", "--tool", "claude", "--open"], &path);
    assert_eq!(code, 0, "`use dual --open` failed: {err}{out}");

    let (after, _, _) = run(root, &["ls"]);
    let marked: Vec<&str> = after.lines().filter(|l| l.contains("pays")).collect();
    assert_eq!(marked.len(), 1, "exactly one row should pay:\n{after}");
    assert!(
        marked[0].contains("dual"),
        "the paying row names someone else:\n{}",
        marked[0]
    );
    assert_eq!(
        live_claude_email(root),
        "oth@example.com",
        "the live claude login was overwritten:\n{after}"
    );
}

/// A switch that did not work does not launch anything.
///
/// `--open` is a switch and then a launch, in that order. Landing in a live
/// session on the account the user was trying to leave is worse than not
/// launching: the next message is billed to the wrong account, and nothing on
/// screen says so.
#[test]
fn open_does_not_launch_a_failed_switch() {
    use std::os::unix::fs::PermissionsExt;
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_claude(root, "uuid-b", "b@example.com");
    run(root, &["add", "beta"]);
    let path = fake_tool(root, "claude", "CLAUDE-OPENED");
    // The live login cannot be written, so the switch cannot happen.
    let live = root.join(".claude");
    std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o500)).unwrap();

    let (out, err, code) =
        run_on_path(root, &["use", "alpha", "--tool", "claude", "--open"], &path);
    std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_ne!(
        code, 0,
        "a switch that could not write should not report success:\n{out}{err}"
    );
    assert!(
        !out.contains("CLAUDE-OPENED"),
        "claude launched after a failed switch:\n{out}{err}"
    );
}

/// Drive the plain numbered menu over a pipe. `TERM=dumb` is what a terminal
/// that cannot render ANSI reports (Emacs shell, some CI), and it is what sends
/// `ui` down this path instead of the full-screen picker.
fn run_ui(root: &Path, keys: &str) -> (String, String, i32) {
    use std::io::Write;
    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(keys.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Drive the real full-screen picker through a pseudo-terminal.
///
/// `SWAPDEX_ASSUME_TTY` deliberately selects the plain fallback; a PTY is
/// required to cover the dashboard row builder that runs on an actual terminal.
///
/// Linux only, and the reason is the HARNESS rather than the product. On the
/// macOS runner the child wrote ZERO bytes to the pty and was still alive
/// thirty seconds later - measured, by dumping what the pty had received. The
/// child is never made a session leader, so the slave never becomes its
/// controlling terminal; Linux tolerates that and macOS does not. Making this
/// portable means `setsid` plus `TIOCSCTTY` in a pre-exec hook, which is a
/// harness to write deliberately, not to bolt on to keep a job green.
#[cfg(target_os = "linux")]
fn run_fullscreen_ui(root: &Path, keys: &[u8]) -> (String, i32) {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // `*mut` for the termios and winsize arguments on BOTH platforms: macOS
    // declares them `*mut` and Linux `*const`, and a `*mut` coerces to a
    // `*const` while the reverse does not. Written the other way round this
    // compiled here and broke the macOS job only. Raw pointers rather than
    // `&mut`, because on Linux clippy rejects a mutable reference the callee
    // does not need.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };

    let mut child = Command::new(bin())
        .arg("ui")
        .env("SWAPDEX_ROOT", root)
        .env("TERM", "xterm")
        .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut input = master.try_clone().unwrap();
    let keys = keys.to_vec();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        input.write_all(&keys).unwrap();
    });

    // Generous, because the macOS runner is far slower than this box and a
    // deadline that merely expires tells you nothing about why.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // Say what the terminal actually received. "Did not exit" alone
            // cannot distinguish a screen that never drew from one that drew
            // and ignored the keys, and a CI-only failure gives no other way
            // to look.
            let mut seen = Vec::new();
            let _ = master.read_to_end(&mut seen);
            let seen = String::from_utf8_lossy(&seen);
            panic!(
                "full-screen ui did not exit after the supplied keys; \
                 the pty received {} bytes:\n{}",
                seen.len(),
                seen.chars().take(2000).collect::<String>()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    writer.join().unwrap();
    let mut bytes = Vec::new();
    let _ = master.read_to_end(&mut bytes);
    (
        String::from_utf8_lossy(&bytes).into_owned(),
        status.code().unwrap_or(-1),
    )
}

/// The full-screen dashboard must list slots for every supported tool.
///
/// Its hand-written join covered Claude and Codex only. A Gemini slot appeared
/// in `ls` and the plain picker but the terminal dashboard showed the empty
/// welcome screen, so the command disagreed with itself based on terminal type.
///
/// Linux only - see `run_fullscreen_ui`. The row builder this covers is not
/// platform-specific; the pty harness is.
#[cfg(target_os = "linux")]
#[test]
fn the_fullscreen_menu_lists_a_gemini_slot_only_account() {
    let t = fixture();
    let root = t.path();
    seed_gemini_slot(root, "kong", "kong@example.com");

    let (out, code) = run_fullscreen_ui(root, b"q");
    assert_eq!(code, 0, "full-screen menu failed:\n{out}");
    assert!(
        out.contains("kong"),
        "the Gemini account listed by ls is missing from the full-screen menu:\n{out}"
    );
}

/// The number the menu printed beside `name`, so a test can pick that row
/// without assuming what order the menu sorts in.
fn menu_number(out: &str, name: &str) -> String {
    out.lines()
        .find(|l| l.contains(name))
        .and_then(|l| l.trim().split(')').next().map(str::to_string))
        .unwrap_or_else(|| panic!("no menu row for {name}:\n{out}"))
}

/// The menu offers the accounts that exist, not the saved snapshots.
///
/// A slot-only install is what the slot model produces, and `ls` lists it and
/// `use` switches it. The menu asked the snapshot store alone, so it told an
/// owner with a working account to start over - and `setup`, the command it
/// sends them to, is not where you go when you already have one.
#[test]
fn the_menu_lists_a_slot_only_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");

    let (ls_out, _, _) = run(root, &["ls"]);
    assert!(
        ls_out.contains("personal"),
        "fixture check - `ls` should list the slot:\n{ls_out}"
    );

    let (out, err, code) = run_ui(root, "\n");
    assert_eq!(code, 0, "menu should open:\n{out}{err}");
    assert!(
        !out.contains("No accounts saved yet"),
        "an install with a registered slot was told it has nothing:\n{out}{err}"
    );
    assert!(
        out.contains("personal"),
        "the account `ls` lists is missing from the menu:\n{out}{err}"
    );
    // A row you cannot check is a row you cannot trust: the slot's own config
    // is the only thing that names it, and `ls` already reads it for this.
    assert!(
        out.contains("personal@example.com"),
        "the slot row named nobody, so a switch to it is unverifiable:\n{out}{err}"
    );
}

/// A snapshot account must not crowd a slot account off the menu.
///
/// With one of each, the menu numbered only the snapshot - so the slot account
/// had no number, and a menu row is the only way to pick one here. `ls` shows
/// both, and the pick itself already goes through the same switch `use` does.
#[test]
fn the_menu_numbers_every_account_ls_shows() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-a", "a@example.com");
    run(root, &["add", "alpha"]);
    seed_slot(root, "beta", "b@example.com");

    let (out, err, code) = run_ui(root, "\n");
    assert_eq!(code, 0, "menu should open:\n{out}{err}");
    assert!(
        out.contains("alpha"),
        "the snapshot account is missing from the menu:\n{out}{err}"
    );
    assert!(
        out.contains("beta"),
        "the slot account is missing from the menu:\n{out}{err}"
    );

    // Listed is not enough: the number beside the row has to switch to it.
    let n = menu_number(&out, "beta");
    let (out2, err2, code2) = run_ui(root, &format!("{n}\n"));
    assert_eq!(code2, 0, "picking the slot row failed:\n{out2}{err2}");
    let (ls_out, _, _) = run(root, &["ls"]);
    assert!(
        ls_out
            .lines()
            .any(|l| l.contains("beta") && l.contains("<- pays")),
        "the menu pick did not switch to the slot account:\n{ls_out}\n{out2}{err2}"
    );
}

/// A slot-only row must keep its tool after the switch.
///
/// The picker found the row through the merged account list, then the
/// post-switch prompt looked the same name up in the snapshot store alone.
/// That made a working slot lose its "new Claude" action immediately after
/// the picker had switched to it.
#[test]
fn the_menu_offers_the_tool_held_by_a_slot_only_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");

    let (out, err, code) = run_ui(root, "1\n\n");
    assert_eq!(code, 0, "menu switch failed:\n{out}{err}");
    assert!(
        out.contains("c new claude"),
        "the slot's tool disappeared from the post-switch menu:\n{out}{err}"
    );
}

/// `add` must not mint a name that means two different accounts.
///
/// `ls` warns on every listing when one name holds a profile and a slot of a
/// DIFFERENT account, because the row shows one and `use` selects the other.
/// `add` is what creates that state, and its own guards against a name meaning
/// two accounts - exit 6 for an occupied profile, exit 7 for a repoint - only
/// ever asked the store.
#[test]
fn add_refuses_a_name_a_different_account_already_holds() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");
    seed_claude(root, "work-uuid", "work@example.com");

    let (out, err, code) = run(root, &["add", "personal", "--tool", "claude-code"]);
    assert_ne!(
        code, 0,
        "add saved over a name a different account's slot already holds:\n{out}{err}"
    );

    // Ask a different command whether the state it left is sound.
    let (ls_out, ls_err, _) = run(root, &["ls"]);
    assert!(
        !ls_err.contains("DIFFERENT"),
        "add created the very state `ls` warns about:\n{ls_out}{ls_err}"
    );
}

/// One name on ONE account is the ordinary case, not a collision.
///
/// `swapdex run <name>` registers a slot, and snapshotting that same login
/// under that same name is what `add` is for. A guard that refused every name a
/// slot holds would break it.
#[test]
fn add_still_snapshots_a_slot_holding_the_same_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");
    seed_claude(root, "personal", "personal@example.com");

    let (out, err, code) = run(root, &["add", "personal", "--tool", "claude-code"]);
    assert_eq!(
        code, 0,
        "add refused to snapshot the account its own slot holds:\n{out}{err}"
    );
}

/// A slot swapdex cannot read holds no account to collide with.
///
/// `swapdex run <name>` registers the slot before anyone logs into it, so the
/// directory is there and empty. Refusing on the bare registry entry would
/// block the first save on every freshly-made slot.
#[test]
fn add_accepts_a_name_whose_slot_holds_nothing_readable() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");
    let dir = root.join(".local/share/swapdex/slots/personal");
    std::fs::remove_file(dir.join(".credentials.json")).unwrap();
    std::fs::remove_file(dir.join(".claude.json")).unwrap();
    seed_claude(root, "work-uuid", "work@example.com");

    let (out, err, code) = run(root, &["add", "personal", "--tool", "claude-code"]);
    assert_eq!(
        code, 0,
        "add refused over a slot holding no readable account:\n{out}{err}"
    );
}

/// `login` must not save a different account over a name held by a slot.
///
/// `add` and `setup` already protect this seam. The interactive login flow
/// repeated the repoint guard against snapshots only, so completing a real
/// sign-in could leave `ls` showing one account while `use` selected another.
#[test]
fn login_rescues_a_new_account_from_a_slot_name_collision() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let t = fixture();
    let root = t.path();
    seed_slot(root, "personal", "personal@example.com");
    seed_claude(root, "old-uuid", "old@example.com");
    let (_, err, code) = run(root, &["add", "old", "--tool", "claude-code"]);
    assert_eq!(code, 0, "could not save the outgoing account: {err}");

    let dir = root.join("fakebin-login-collision");
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("claude");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
mkdir -p "$SWAPDEX_ROOT/.claude"
printf '%s' '{"claudeAiOauth":{"accessToken":"AT-NEW","refreshToken":"RT-NEW","expiresAt":9999999999999,"subscriptionType":"max"}}' > "$SWAPDEX_ROOT/.claude/.credentials.json"
printf '%s' '{"oauthAccount":{"accountUuid":"new-uuid","emailAddress":"new@example.com"}}' > "$SWAPDEX_ROOT/.claude.json"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut child = Command::new(bin())
        .args(["login", "personal", "--tool", "claude-code"])
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_ASSUME_TTY", "1")
        .env("PATH", path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"y\nrescued\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code().unwrap_or(-1),
        0,
        "login failed:\n{out}{err}"
    );
    assert!(
        out.contains("saved profile 'rescued'"),
        "the new login was not rescued under a free name:\n{out}{err}"
    );

    let (listing, listing_err, _) = run(root, &["ls"]);
    assert!(
        !listing_err.contains("DIFFERENT"),
        "login minted the crossed name that ls warns about:\n{listing}{listing_err}"
    );
    assert!(
        listing.contains("personal") && listing.contains("rescued"),
        "both accounts should remain reachable under distinct names:\n{listing}{listing_err}"
    );
}

/// Drive the interactive wizard over a pipe. `SWAPDEX_ASSUME_TTY` is the escape
/// hatch `setup` already carries for exactly this.
fn run_stdin(root: &Path, args: &[&str], input: &str) -> (String, String, i32) {
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Attaching a tool to a profile must still respect a same-named slot.
///
/// Setup normally sends names through the slot collision guard, but its
/// automatic multi-tool attach branch returned early. A Codex profile and a
/// different Claude slot named `work` therefore made setup file the current
/// Claude login under the slot's name without asking for a safe alternative.
#[test]
fn setup_does_not_auto_attach_over_a_different_slots_name() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "personal@example.com");
    seed_live_codex(root, "codex@example.com");
    let (_, err, code) = run(root, &["add", "work", "--tool", "codex"]);
    assert_eq!(code, 0, "could not seed the Codex profile: {err}");
    seed_claude(root, "work-uuid", "work@example.com");

    let (out, err, code) = run_stdin(root, &["setup"], "safe\n\n");
    assert_eq!(code, 0, "setup failed:\n{out}{err}");
    assert!(
        out.contains("saved as 'safe'"),
        "setup did not ask for a name clear of the existing slot:\n{out}{err}"
    );

    let (listing, listing_err, _) = run(root, &["ls"]);
    assert!(
        !listing_err.contains("DIFFERENT"),
        "setup auto-attached over a different slot account:\n{listing}{listing_err}"
    );
}

/// Setup's summary must acknowledge accounts already held in slots.
///
/// A slot-only account is ready for `use`, `ls`, and `rm`. Summarising the
/// snapshot store alone called that same machine empty at the end of setup.
#[test]
fn setup_recognises_an_existing_slot_only_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");

    let (out, err, code) = run_stdin(root, &["setup"], "n\n");
    assert_eq!(code, 0, "setup failed:\n{out}{err}");
    assert!(
        !out.contains("No accounts saved yet"),
        "setup called the slot account nonexistent:\n{out}{err}"
    );
    assert!(
        out.contains("You're set - saved: work."),
        "setup did not acknowledge the account every other command sees:\n{out}{err}"
    );
}

/// Onboarding must not call a saved snapshot "no accounts".
///
/// Declining migration leaves the old account usable through `use`, `ls`, and
/// `rm`. The final summary counted permanent slots alone and contradicted all
/// three commands about whether that account still existed.
#[test]
fn onboard_recognises_a_snapshot_account_that_was_not_migrated() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "work-uuid", "work@example.com");
    let (_, err, code) = run(root, &["add", "work", "--tool", "claude-code"]);
    assert_eq!(code, 0, "could not seed the saved account: {err}");

    let (out, err, code) = run(root, &["onboard"]);
    assert_eq!(code, 0, "onboard failed:\n{out}{err}");
    assert!(
        !out.contains("No accounts yet"),
        "onboard called the snapshot account nonexistent:\n{out}{err}"
    );
    assert!(
        out.contains("swapdex ui` shows your accounts"),
        "onboard did not acknowledge the account every other command sees:\n{out}{err}"
    );
}

/// `setup` must not mint the name `ls` warns about either.
///
/// `add` learned to refuse a name a slot holds for a different account, but the
/// wizard names accounts through `ask_name`, which asks the snapshot store
/// alone. It is the worse door: the name it collides on is the one it SUGGESTS
/// as the default, so pressing Enter is enough.
#[test]
fn setup_refuses_a_name_a_different_account_already_holds() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "personal@example.com");
    seed_claude(root, "work-uuid", "work@example.com");

    // Enter accepts the suggested [work]; 'skip' leaves the tool unsaved.
    let (out, err, code) = run_stdin(root, &["setup"], "\nskip\n");
    assert_eq!(code, 0, "setup failed:\n{out}{err}");

    let (ls_out, ls_err, _) = run(root, &["ls"]);
    assert!(
        !ls_err.contains("DIFFERENT"),
        "setup minted the crossed name ls warns about:\n{out}\n--- ls ---\n{ls_out}{ls_err}"
    );
}

/// One name on ONE account is still the ordinary case.
///
/// A slot registered by `swapdex run <name>` and the login you are sitting on
/// are usually the same account. Saving a snapshot of it under that name is
/// what the wizard is for; a guard that refused every name a slot holds would
/// break the common path.
#[test]
fn setup_still_saves_under_a_slot_holding_the_same_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    seed_claude(root, "work", "work@example.com");

    let (out, err, code) = run_stdin(root, &["setup"], "\n");
    assert_eq!(code, 0, "setup failed:\n{out}{err}");
    assert!(
        out.contains("saved as 'work'"),
        "setup refused a slot holding the SAME account:\n{out}{err}"
    );
}

/// A slot swapdex cannot read holds no account to collide with.
///
/// `swapdex run <name>` registers the slot before anyone logs into it. Treating
/// an unreadable slot as a different account would refuse the wizard's own
/// suggestion on every freshly-made slot, with no name it would accept.
#[test]
fn setup_accepts_a_name_whose_slot_holds_nothing_readable() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let dir = root.join(".local/share/swapdex/slots/work");
    std::fs::remove_file(dir.join(".credentials.json")).unwrap();
    std::fs::remove_file(dir.join(".claude.json")).unwrap();
    seed_claude(root, "work-uuid", "work@example.com");

    let (out, err, code) = run_stdin(root, &["setup"], "\n");
    assert_eq!(code, 0, "setup failed:\n{out}{err}");
    assert!(
        out.contains("saved as 'work'"),
        "setup refused over a slot holding no readable account:\n{out}{err}"
    );
}

/// Ask the MCP server for its account list the way an agent would.
fn mcp_list_accounts(root: &Path) -> Vec<serde_json::Value> {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_accounts"}}"#,
        "\n",
    );
    let (out, err, code) = run_stdin(root, &["mcp"], input);
    assert_eq!(code, 0, "mcp exited {code}:\n{out}{err}");
    let reply = out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 2)
        .unwrap_or_else(|| panic!("no reply to list_accounts:\n{out}{err}"));
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("list_accounts returned no text: {reply}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not a JSON array: {text}: {e}"))
}

/// The MCP list must name the accounts a switch can name.
///
/// An account can live as a registered slot alone - `swapdex run <name>` makes
/// exactly that - and `ls`, `use` and `rm` all merge both registries. The tool
/// read the snapshot store alone, so an agent asking swapdex what accounts
/// exist was told none on a machine whose accounts are slots.
#[test]
fn mcp_lists_a_slot_only_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");

    let rows = mcp_list_accounts(root);
    assert!(
        rows.iter().any(|r| r["name"] == "work"),
        "list_accounts hid a slot-only account: {rows:?}"
    );
}

/// An account held as both a snapshot and a slot is still one account.
///
/// The two registries overlap, so a listing that merges them by concatenation
/// names the same account twice and an agent counts two logins where there is
/// one.
#[test]
fn mcp_lists_an_account_held_both_ways_once() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "work-uuid", "work@example.com");
    let (o, e, c) = run(root, &["add", "work"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");
    seed_slot(root, "work", "work@example.com");

    let rows = mcp_list_accounts(root);
    let held: Vec<_> = rows.iter().filter(|r| r["name"] == "work").collect();
    assert_eq!(held.len(), 1, "'work' listed twice: {rows:?}");
}

/// Widening the source must not cost the field that says which one is live.
#[test]
fn mcp_still_reports_active_tools_for_a_live_account() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "home-uuid", "me@example.com");
    let (o, e, c) = run(root, &["add", "home"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");

    let rows = mcp_list_accounts(root);
    let row = rows
        .iter()
        .find(|r| r["name"] == "home")
        .unwrap_or_else(|| panic!("'home' missing: {rows:?}"));
    assert_eq!(
        row["active_tools"],
        serde_json::json!(["claude-code"]),
        "active_tools lost: {row}"
    );
}

/// A codex slot with its account and refresh token spelled out.
///
/// `seed_codex_slot` fixes both, which is what most tests want. A rotation is
/// only visible when two holders of ONE account differ in the refresh token, so
/// these tests have to set the pair themselves.
fn seed_codex_slot_as(root: &Path, name: &str, email: &str, account_id: &str, refresh: &str) {
    let dir = root.join(".local/share/swapdex/slots").join(name);
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "last_refresh": "2026-09-03T02:56:06Z",
            "tokens": {"id_token": codex_id_token(email), "access_token": "AT",
                       "refresh_token": refresh, "account_id": account_id}}))
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

/// A saved copy-model codex snapshot in the store.
fn seed_codex_snapshot(root: &Path, name: &str, email: &str, account_id: &str, refresh: &str) {
    let d = root
        .join(".local/share/swapdex/accounts")
        .join(name)
        .join("codex");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("auth"),
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "last_refresh": "2026-09-03T02:56:06Z",
            "tokens": {"id_token": codex_id_token(email), "access_token": "AT",
                       "refresh_token": refresh, "account_id": account_id}}))
        .unwrap(),
    )
    .unwrap();
    chmod600(&d.join("auth"));
}

/// A snapshot whose refresh token was rotated away is dead, not merely old.
///
/// These refresh tokens are single-use: redeeming one retires the copy every
/// other holder is carrying. So when a live slot and a saved snapshot name the
/// SAME account but differ in the refresh token, the snapshot's token is one
/// the server has already retired, and restoring it hands the tool a login that
/// cannot renew itself. The wall-clock staleness check cannot see this - it
/// measures how long ago the snapshot was written, and a rotation can retire a
/// token minutes after it is saved.
///
/// A real machine sat in exactly this state for six days with nothing said: the
/// slot held a live pair, the snapshot and the tool's own dir held the retired
/// one, and the failure surfaced only when the access token lapsed and the
/// renewal it then attempted was refused.
#[test]
fn use_refuses_a_codex_snapshot_whose_refresh_token_was_rotated_away() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot_as(root, "live", "shared@example.com", "acct-shared", "RT-NEW");
    seed_codex_snapshot(root, "stale", "shared@example.com", "acct-shared", "RT-OLD");

    let (out, err, code) = run(root, &["use", "stale", "--tool", "codex"]);
    let said = format!("{out}{err}");
    assert_ne!(
        code, 0,
        "a retired snapshot was restored as a success:\n{said}"
    );
    assert!(
        said.contains("rotated") || said.contains("retired") || said.contains("revoked"),
        "nothing said the refresh token was retired by another holder:\n{said}"
    );
}

/// The rotation check must not fire on the copy that IS current.
///
/// Two holders of one account are normal - a slot and the snapshot it was
/// captured from agree until something rotates. A check that refused those too
/// would refuse every healthy switch and get taken back out.
#[test]
fn a_codex_snapshot_holding_the_live_refresh_token_still_switches() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot_as(root, "live", "shared@example.com", "acct-shared", "RT-SAME");
    seed_codex_snapshot(root, "twin", "shared@example.com", "acct-shared", "RT-SAME");

    let (out, err, code) = run(root, &["use", "twin", "--tool", "codex"]);
    assert_eq!(
        code, 0,
        "a snapshot holding the live token was refused:\n{out}{err}"
    );
}

/// `ls` must mark the account swapdex is actually pointing the tool at.
///
/// The per-tool mark was read from the tool's OWN config dir, which the slot
/// model does not write: switching repoints a pointer and leaves the tool's dir
/// alone. On a machine whose dir was left behind by an earlier copy-model
/// switch, `ls` therefore marked the abandoned account active while every
/// switch, the proxy and the serving pointer all named a different one.
#[test]
fn the_codex_active_marker_follows_the_slot_pointer() {
    let t = fixture();
    let root = t.path();
    // An orphan login in the tool's own dir, saved as a profile so the mark has
    // a name to land on.
    seed_live_codex(root, "orphan@example.com");
    let (o, e, c) = run(root, &["add", "orphan"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");
    seed_codex_slot(root, "cx-one", "one@example.com");
    let (_, err, code) = run(root, &["use", "cx-one", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, _, _) = run(root, &["ls"]);
    let row = |n: &str| {
        out.lines()
            .find(|l| l.contains(n))
            .unwrap_or("")
            .to_string()
    };
    assert!(
        row("cx-one").contains("codex*"),
        "the pointer names cx-one but ls does not mark it:\n{out}"
    );
    assert!(
        !row("orphan").contains("codex*"),
        "ls marks an orphaned tool dir as the active codex account:\n{out}"
    );
}

/// A tool dir that disagrees with the pointer is news, not something to hide.
///
/// Reading the mark off the pointer alone would make the orphan invisible - and
/// an unshimmed launch still lands on whatever the tool's own dir holds. The
/// disagreement is the thing a reader needs, since it is the state in which the
/// account swapdex reports and the account that actually pays are different.
#[test]
fn a_tool_dir_that_disagrees_with_the_pointer_is_reported() {
    let t = fixture();
    let root = t.path();
    seed_live_codex(root, "orphan@example.com");
    let (o, e, c) = run(root, &["add", "orphan"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");
    seed_codex_slot(root, "cx-one", "one@example.com");
    let (_, err, code) = run(root, &["use", "cx-one", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err, _) = run(root, &["ls"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("orphan"),
        "the abandoned login in the tool's own dir is never mentioned:\n{said}"
    );
    assert!(
        said.contains("not the account") || said.contains("would launch") || said.contains("stale"),
        "nothing explains that an unshimmed launch lands somewhere else:\n{said}"
    );
}

/// The disagreement note must stay silent when the two agree.
///
/// It fires on a comparison, and a note that also fired when the pointer and
/// the tool's own dir named the SAME account would tell every healthy machine
/// that a plain launch goes somewhere else. That reads as a warning about
/// nothing, and a warning about nothing is what gets a real one ignored.
#[test]
fn a_tool_dir_that_agrees_with_the_pointer_is_not_reported() {
    let t = fixture();
    let root = t.path();
    seed_live_codex(root, "same@example.com");
    let (o, e, c) = run(root, &["add", "same"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");
    // The slot carries the profile's own name, so the pointer and the tool's
    // dir resolve to one account.
    seed_codex_slot(root, "same", "same@example.com");
    let (_, err, code) = run(root, &["use", "same", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err, _) = run(root, &["ls"]);
    let said = format!("{out}{err}");
    assert!(
        !said.contains("would launch"),
        "the note fired although the pointer and the tool's dir agree:\n{said}"
    );
}

/// Every reader of the listing must name the same active account.
///
/// On a machine whose accounts live only as slots, the human listing starred a
/// row and marked it `<- pays` while `ls --json` reported every row with an
/// empty `active_tools`, and the MCP account list said the same nothing. The
/// per-tool mark was read from the tool's own config dir, which such a machine
/// does not have - so the two listings disagreed about whether any account was
/// active at all, and anything reading the JSON could not name the one paying.
#[test]
fn every_listing_names_the_same_active_tool() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (human, _, _) = run(root, &["ls"]);
    assert!(
        human.contains("claude-code*"),
        "the human listing marks no active tool:\n{human}"
    );

    let (json, _, _) = run(root, &["ls", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("ls --json is not json ({e}): {json}"));
    let row = rows
        .as_array()
        .and_then(|a| a.iter().find(|r| r["name"] == "work"))
        .unwrap_or_else(|| panic!("no row for work: {json}"));
    assert_eq!(
        row["active_tools"],
        serde_json::json!(["claude-code"]),
        "`ls --json` does not name the tool the human listing marks: {row}"
    );

    let mcp = mcp_list_accounts(root);
    let mrow = mcp
        .iter()
        .find(|r| r["name"] == "work")
        .unwrap_or_else(|| panic!("no mcp row for work: {mcp:?}"));
    assert_eq!(
        mrow["active_tools"],
        serde_json::json!(["claude-code"]),
        "the MCP account list does not name it either: {mrow}"
    );
}

/// `status` and `ls` must name the same active account.
///
/// `status` is documented as "the active account per tool" - the question `ls`
/// answers with its star. It read the tool's own config dir, which the slot
/// model never writes, so on a machine whose accounts live only as slots it
/// said "not logged in" while `ls` starred an account and `serve` was serving
/// it. Both ship as statusline commands, so the contradiction went straight
/// into people's prompts.
#[test]
fn status_names_the_account_ls_marks_active() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (human, _, _) = run(root, &["ls"]);
    assert!(
        human.contains("claude-code*"),
        "ls marks no active account:\n{human}"
    );

    let (st, _, _) = run(root, &["status"]);
    assert!(
        !st.contains("claude-code: not logged in"),
        "status says not logged in while ls marks an account active:\n{st}"
    );
    assert!(
        st.contains("work"),
        "status does not name the active account:\n{st}"
    );
}

/// The compact line is the one that lands in a shell prompt.
#[test]
fn status_short_names_the_slot_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (short, _, _) = run(root, &["status", "--short"]);
    assert!(
        short.contains("claude:work"),
        "the compact line does not name the active account: {short:?}"
    );
}

/// Anything parsing `status --json` needs the same answer the screen gives.
#[test]
fn status_json_reports_the_slot_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (json, _, _) = run(root, &["status", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("status --json is not json ({e}): {json}"));
    let row = rows
        .as_array()
        .and_then(|a| a.iter().find(|r| r["tool"] == "claude-code"))
        .unwrap_or_else(|| panic!("no claude-code row: {json}"));
    assert_eq!(
        row["logged_in"],
        serde_json::json!(true),
        "status --json calls an active slot account logged out: {row}"
    );
    assert_eq!(
        row["profile"],
        serde_json::json!("work"),
        "status --json does not name the active profile: {row}"
    );
}

/// A machine with no slots must still report what its tool dir holds.
///
/// The pointer is the better answer only where there is one. Reading it in
/// preference must not cost the copy-model machine its own answer, nor the
/// honest "not logged in" on a machine that really has no login.
#[test]
fn status_still_reports_a_live_login_when_no_slot_points_anywhere() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-live", "live@example.com");
    let (o, e, c) = run(root, &["add", "live"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");

    let (st, _, _) = run(root, &["status"]);
    assert!(
        st.contains("live@example.com"),
        "status lost the live login on a machine with no slots:\n{st}"
    );
    let (short, _, _) = run(root, &["status", "--short"]);
    assert!(
        short.contains("claude:live"),
        "the compact line lost it too: {short:?}"
    );
}

/// Following the pointer must not hide what the tool's own dir holds.
///
/// The two answer different questions and both are true: the pointer says who
/// swapdex serves, the tool's dir says who an unshimmed launch would use. A
/// `status` that printed only the first would be exactly as misleading as the
/// one that printed only the second - it would just be wrong in the other
/// direction, and the abandoned login that cost a scheduled job 42 hours would
/// stay off the screen that claims to say who is active.
#[test]
fn status_names_the_abandoned_login_too() {
    let t = fixture();
    let root = t.path();
    seed_live_codex(root, "orphan@example.com");
    let (o, e, c) = run(root, &["add", "orphan"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");
    seed_codex_slot(root, "cx-one", "one@example.com");
    let (_, err, code) = run(root, &["use", "cx-one", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (st, _, _) = run(root, &["status"]);
    assert!(
        st.contains("cx-one"),
        "status does not name the account the pointer serves:\n{st}"
    );
    assert!(
        st.contains("orphan"),
        "status hides the login the tool's own dir still holds:\n{st}"
    );
}

/// One `tools/call` against the MCP server, with its text payload parsed.
fn mcp_call(root: &Path, tool: &str) -> serde_json::Value {
    let input = format!(
        "{}\n{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"{tool}\"}}}}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#
    );
    let (out, err, code) = run_stdin(root, &["mcp"], &input);
    assert_eq!(code, 0, "mcp exited {code}:\n{out}{err}");
    let reply = out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 2)
        .unwrap_or_else(|| panic!("no reply to {tool}:\n{out}{err}"));
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool} returned no text: {reply}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{tool} text is not JSON: {text}: {e}"))
}

fn mcp_whoami(root: &Path) -> Vec<serde_json::Value> {
    mcp_call(root, "whoami")
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("whoami did not return an array"))
}

/// One MCP server must not give two answers to "is anyone signed in".
///
/// `list_accounts` reports `active_tools` from the pointer, while `whoami` read
/// the tool's own config dir - which a slot switch never writes. So an agent
/// that asked swapdex what accounts it had, and then asked who was signed in,
/// got a list of accounts and a flat denial that any of them was: the two calls
/// are answered by the same process, from the same store, one line apart.
#[test]
fn mcp_whoami_agrees_with_mcp_list_accounts() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let rows = mcp_list_accounts(root);
    let listed = rows
        .iter()
        .find(|r| r["name"] == "work")
        .unwrap_or_else(|| panic!("no row for work: {rows:?}"));
    assert_eq!(
        listed["active_tools"],
        serde_json::json!(["claude-code"]),
        "list_accounts does not call it active: {listed}"
    );

    let who = mcp_whoami(root);
    let claude = who
        .iter()
        .find(|r| r["tool"] == "claude-code")
        .unwrap_or_else(|| panic!("whoami has no claude-code row: {who:?}"));
    assert_ne!(
        claude["display"],
        serde_json::json!("not logged in"),
        "whoami denies the account list_accounts calls active: {claude}"
    );
    assert!(
        claude.to_string().contains("work"),
        "whoami does not name the active account: {claude}"
    );
}

/// Preferring the pointer must not cost a copy-model machine its answer.
///
/// `whoami` is the call an agent makes to find out whose login it is talking
/// to. Reading the pointer first is right where there is one; a machine with no
/// slots still has a live login, and answering "not logged in" there would be
/// the same defect pointing the other way.
#[test]
fn mcp_whoami_still_reports_a_live_login_when_no_slot_points_anywhere() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-live", "live@example.com");
    let (o, e, c) = run(root, &["add", "live"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");

    let who = mcp_whoami(root);
    let claude = who
        .iter()
        .find(|r| r["tool"] == "claude-code")
        .unwrap_or_else(|| panic!("whoami has no claude-code row: {who:?}"));
    assert_ne!(
        claude["display"],
        serde_json::json!("not logged in"),
        "whoami lost the live login on a machine with no slots: {claude}"
    );
    assert_eq!(
        claude["email"],
        serde_json::json!("live@example.com"),
        "whoami does not name the live login: {claude}"
    );
}

/// `doctor` must not deny the account its own next line names.
///
/// A Codex login refreshed days ago is current, and doctor must say nothing.
///
/// This is the units guard. Claude records a deadline in MILLISECONDS and Codex
/// records `last_refresh` as RFC 3339 seconds, so the reader scales one and not
/// the other. Get that wrong and every healthy Codex slot reports roughly fifty
/// years idle - which an ancient-timestamp test cannot catch, because seconds
/// and milliseconds both clear the threshold there. Only a fresh login
/// separates them.
#[test]
fn doctor_does_not_call_a_freshly_refreshed_codex_login_idle() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cx", "cx@example.com");
    let cred = root.join(".local/share/swapdex/slots/cx/auth.json");
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&cred).unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    v["last_refresh"] = serde_json::json!(rfc3339_utc(now));
    std::fs::write(&cred, serde_json::to_vec(&v).unwrap()).unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        !said.contains("idle"),
        "doctor calls a login refreshed just now idle:\n{said}"
    );
}

/// The other direction: a Codex login that has sat unrefreshed past the
/// threshold must be flagged, because by then the refresh token itself may have
/// been rotated away and the account is dead without saying so.
#[test]
fn doctor_flags_a_codex_login_that_has_sat_unrefreshed() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cx", "cx@example.com");
    let cred = root.join(".local/share/swapdex/slots/cx/auth.json");
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&cred).unwrap()).unwrap();
    let long_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 200 * 86_400;
    v["last_refresh"] = serde_json::json!(rfc3339_utc(long_ago));
    std::fs::write(&cred, serde_json::to_vec(&v).unwrap()).unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("slot:cx") && said.contains("idle"),
        "doctor says nothing about a Codex login unrefreshed for 200 days:\n{said}"
    );
}

/// The over-correction of the test below: a credential that is THERE but will
/// not parse is not a missing login.
///
/// Claude's reader documents this - unreadable is "treated as healthy rather
/// than guessed at" - because the remedy differs. "No login yet, run once to
/// sign in" sent at an account that is signed in tells the user to redo work
/// they already did, and buries the real problem (a damaged file) under it.
#[test]
fn doctor_does_not_call_an_unparseable_codex_credential_a_missing_login() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cx", "cx@example.com");
    let cred = root.join(".local/share/swapdex/slots/cx/auth.json");
    assert!(cred.exists(), "the fixture wrote no credential");
    std::fs::write(&cred, b"{ this is not json").unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        !said.contains("no login yet"),
        "doctor calls a present-but-damaged credential a missing login:\n{said}"
    );
}

/// Register a slot for `tool` whose directory holds no credential at all.
fn seed_empty_codex_slot(root: &Path, name: &str) {
    seed_codex_slot(root, name, "unused@example.com");
    let dir = root.join(".local/share/swapdex/slots").join(name);
    std::fs::remove_file(dir.join("auth.json")).unwrap();
}

/// `status` must read the identity in a Gemini slot, not call it unreadable.
///
/// `any_slot_email` promises to read whichever tool wrote the slot, but stopped
/// after adding Codex. Gemini records the active address in
/// `google_accounts.json`, so hiding it makes a healthy active login look
/// damaged on the screen whose job is to say who is active.
#[test]
fn status_reads_a_gemini_slots_recorded_email() {
    let t = fixture();
    let root = t.path();
    seed_gemini_slot(root, "gwork", "g@example.com");
    let (_, err, code) = run(root, &["use", "gwork", "--tool", "gemini"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err, code) = run(root, &["status"]);
    assert_eq!(code, 0, "status failed: {err}");
    assert!(
        out.contains("gemini: g@example.com (profile 'gwork')"),
        "status calls a named Gemini login unreadable:\n{out}{err}"
    );
}

/// An address in Gemini's `old` list is not the slot's current identity.
///
/// Extending the cross-tool reader one field too far would put a previous login
/// beside the active slot and turn missing information into wrong information.
#[test]
fn mcp_whoami_does_not_call_an_old_gemini_account_active() {
    let t = fixture();
    let root = t.path();
    seed_gemini_slot(root, "gwork", "current@example.com");
    let accounts = root.join(".local/share/swapdex/slots/gwork/google_accounts.json");
    std::fs::write(accounts, br#"{"old":["old@example.com"]}"#).unwrap();
    let (_, err, code) = run(root, &["use", "gwork", "--tool", "gemini"]);
    assert_eq!(code, 0, "use failed: {err}");

    let who = mcp_whoami(root);
    let gemini = who
        .iter()
        .find(|r| r["tool"] == "gemini")
        .unwrap_or_else(|| panic!("whoami has no Gemini row: {who:?}"));
    assert_eq!(
        gemini["email"],
        serde_json::Value::Null,
        "whoami called an old Gemini login active: {gemini}"
    );
}

/// An old, present login is refreshed by running its own tool, not signed in
/// from scratch; the remedy must preserve both that verb and Codex's flag.
#[test]
fn doctor_names_codex_in_the_remedy_for_an_idle_codex_login() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cx", "cx@example.com");
    let cred = root.join(".local/share/swapdex/slots/cx/auth.json");
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&cred).unwrap()).unwrap();
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 200 * 86_400;
    v["last_refresh"] = serde_json::json!(rfc3339_utc(old));
    std::fs::write(&cred, serde_json::to_vec(&v).unwrap()).unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("`swapdex run cx --tool codex` once refreshes it"),
        "doctor gives the wrong action for an idle Codex login:\n{said}"
    );
}

/// Claude remains `run`'s default even on the idle-login branch.
#[test]
fn doctor_leaves_claudes_default_out_of_an_idle_login_remedy() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "cl", "cl@example.com");
    let cred = root.join(".local/share/swapdex/slots/cl/.credentials.json");
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&cred).unwrap()).unwrap();
    v["claudeAiOauth"]["expiresAt"] = serde_json::json!(1_600_000_000_000i64);
    std::fs::write(&cred, serde_json::to_vec(&v).unwrap()).unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("`swapdex run cl` once refreshes it"),
        "doctor no longer names the default Claude refresh:\n{said}"
    );
    assert!(
        !said.contains("`swapdex run cl --tool"),
        "doctor redundantly spells Claude's default on the idle branch:\n{said}"
    );
}

/// `ls` marks who pays. An account with no login cannot.
///
/// `serve` refuses to put one there and says why: "it cannot pay for turns -
/// your own account would, while every screen named '<name>'". `payer_line`
/// exists so the reason travels with the name, and Codex's /status line uses
/// it. `ls` marked the row `<- pays` with no login check at all, so a glance
/// (and a script) read an account as footing the bill while the proxy was
/// quietly forwarding the reader's own credential.
#[test]
fn ls_does_not_say_an_account_with_no_login_pays() {
    let t = fixture();
    let root = t.path();
    seed_empty_codex_slot(root, "cxwork");
    let (_, err, code) = run(root, &["use", "cxwork", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err2, _) = run(root, &["ls"]);
    let said = format!("{out}{err2}");
    assert!(
        said.contains("cxwork"),
        "ls no longer lists the account, so this measures nothing:\n{said}"
    );
    assert!(
        said.contains("pays"),
        "ls no longer marks a payer, so this measures nothing:\n{said}"
    );
    assert!(
        said.contains("no login"),
        "ls says an account with no login pays, without saying it cannot:\n{said}"
    );
}

/// The over-correction: a payer that IS signed in must not be slandered.
#[test]
fn ls_does_not_claim_a_signed_in_payer_has_no_login() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cxwork", "cx@example.com");
    let (_, err, code) = run(root, &["use", "cxwork", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err2, _) = run(root, &["ls"]);
    let said = format!("{out}{err2}");
    assert!(said.contains("pays"), "ls no longer marks a payer:\n{said}");
    assert!(
        !said.contains("no login"),
        "ls calls a signed-in payer loginless:\n{said}"
    );
}

/// `short_line` goes in a shell prompt, and its own contract says "None when
/// nothing is logged in".
///
/// It took the pointer without asking whether that slot holds a credential, so
/// a prompt read `codex:cxwork` on a machine where nothing is signed in at all
/// and `serve` refuses that account for having no login. The comment right
/// above the branch states the rule it broke: a prompt naming the account
/// swapdex is not serving is worse than no line at all.
#[test]
fn the_short_line_does_not_name_an_account_with_no_login() {
    let t = fixture();
    let root = t.path();
    seed_empty_codex_slot(root, "cxwork");
    let (_, err, code) = run(root, &["use", "cxwork", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err2, _) = run(root, &["status", "--short"]);
    let said = format!("{out}{err2}");
    assert!(
        !said.contains("cxwork"),
        "the prompt line names an account with no login:\n{said}"
    );
    assert!(
        said.contains("not signed in"),
        "nothing is signed in on this machine; the line should say so:\n{said}"
    );
}

/// The over-correction: a slot that IS signed in still belongs in the prompt.
#[test]
fn the_short_line_names_a_slot_that_is_signed_in() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err2, _) = run(root, &["status", "--short"]);
    let said = format!("{out}{err2}");
    assert!(
        said.contains("claude:work"),
        "the prompt line dropped a slot that is signed in:\n{said}"
    );
}

/// The other direction: a slot that IS signed in shows its identity.
///
/// Without this, wording the not-signed-in case could swallow the signed-in one
/// and every existing check would stay green - `doctor_agrees_with_the_default_
/// it_reports` only asks that the row contain the account name, which the
/// remedy text "`swapdex run work` signs it in" also does.
#[test]
fn status_shows_the_identity_of_a_slot_that_is_signed_in() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err2, _) = run(root, &["status"]);
    let said = format!("{out}{err2}");
    assert!(
        said.contains("work@example.com"),
        "status hides the email of a slot that is signed in:\n{said}"
    );
    assert!(
        !said.contains("not signed in"),
        "status calls a signed-in slot unsigned:\n{said}"
    );
}

/// The pointer naming an account is not the account being signed in.
///
/// `slot_active_identity` answers "who does the pointer name, and what identity
/// does that slot carry". Three screens read it as "who is logged in", and none
/// of them asked whether the slot holds a credential - so a registered Codex
/// slot with an EMPTY directory was reported as logged in. `serve` gets this
/// right about the same account ("has no codex login"), and `proxy::has_login`
/// is the helper that knows.
#[test]
fn status_does_not_call_an_empty_slot_signed_in() {
    let t = fixture();
    let root = t.path();
    seed_empty_codex_slot(root, "cxwork");
    let (_, err, code) = run(root, &["use", "cxwork", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    // The surface that already answers correctly. Without it this measures
    // nothing: it is the disagreement that makes the others wrong.
    let (so, se, _) = run(root, &["serve", "cxwork", "--tool", "codex"]);
    assert!(
        format!("{so}{se}").contains("no codex login"),
        "serve no longer reports the missing login, so there is nothing to disagree with"
    );

    let (out, err2, _) = run(root, &["status"]);
    let said = format!("{out}{err2}");
    assert!(
        !said.contains("login unreadable in the slot"),
        "status calls an empty slot's login unreadable rather than absent:\n{said}"
    );

    let (jout, jerr, _) = run(root, &["status", "--json"]);
    let rows: serde_json::Value =
        serde_json::from_str(&jout).unwrap_or_else(|e| panic!("status --json: {e}\n{jout}{jerr}"));
    let codex = rows
        .as_array()
        .and_then(|a| a.iter().find(|r| r["tool"] == "codex"))
        .unwrap_or_else(|| panic!("no codex row:\n{jout}"));
    assert_eq!(
        codex["logged_in"], false,
        "status --json reports an empty slot as logged in:\n{codex}"
    );
}

/// doctor said both things at once about the same account:
///
///   codex         ok - login unreadable in the slot (profile 'cxwork')
///   slot:cxwork   ok - no login yet - ...
///
/// which is the contradiction the comment above doctor's own pointer branch was
/// written about, in the other direction.
#[test]
fn doctor_does_not_contradict_itself_about_an_empty_slot() {
    let t = fixture();
    let root = t.path();
    seed_empty_codex_slot(root, "cxwork");
    let (_, err, code) = run(root, &["use", "cxwork", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, err2, _) = run(root, &["doctor"]);
    let said = format!("{out}{err2}");
    assert!(
        said.contains("slot:cxwork"),
        "the per-slot row is gone, so this measures nothing:\n{said}"
    );
    // The agreement itself, not one wording of the disagreement: the tool row
    // must say the same thing the slot row does. Asserting only the absence of
    // the old phrase let a mutant through that swapped it for "signed in,
    // email unreadable" - still the contradiction this test is named for.
    let codex_row = said
        .lines()
        .find(|l| l.starts_with("codex"))
        .unwrap_or_else(|| panic!("doctor has no codex row:\n{said}"));
    assert!(
        codex_row.contains("not signed in"),
        "doctor's codex row claims a login one row above reporting none:\n{said}"
    );
}

/// The per-slot row names `run`, and `run` defaults to Claude.
///
/// That walk was Claude-only until it was generalised across tools; the message
/// came along unchanged, so a Codex slot was told `swapdex run <name>`.
#[test]
fn the_slot_login_row_names_the_tool() {
    let t = fixture();
    let root = t.path();
    seed_empty_codex_slot(root, "cxwork");

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("swapdex run cxwork"),
        "the row no longer names `run`, so this measures nothing:\n{said}"
    );
    assert!(
        !said.contains("swapdex run cxwork`"),
        "the Codex slot's remedy names a command that runs Claude:\n{said}"
    );
}

/// `import` tells the reader each imported account still needs a sign-in, and
/// named `swapdex run <name>` as a placeholder - with no tool in it.
///
/// The manifest carries a tool per account and `import` creates slots for all
/// of them, so for a Codex account the substituted command launches Claude and
/// leaves a second account of that name behind. The plan already knows the
/// tool; printing the real commands says it without the reader guessing.
#[test]
fn the_import_sign_in_line_names_each_accounts_tool() {
    let t = fixture();
    let root = t.path();
    let manifest = root.join("setup.json");
    std::fs::write(
        &manifest,
        br#"{"version":1,"accounts":[{"name":"cxwork","tool":"codex"},{"name":"clwork","tool":"claude-code"}]}"#,
    )
    .unwrap();

    let (out, err, code) = run(root, &["import", manifest.to_str().unwrap()]);
    assert_eq!(code, 0, "import failed:\n{out}{err}");
    let said = format!("{out}{err}");
    assert!(
        said.contains("still needs its own sign-in"),
        "import no longer mentions signing in, so this measures nothing:\n{said}"
    );
    assert!(
        said.contains("swapdex run cxwork --tool codex"),
        "the sign-in named for an imported Codex account runs Claude:\n{said}"
    );
    assert!(
        said.contains("swapdex run clwork"),
        "the Claude account was not named at all:\n{said}"
    );
    // Not `contains("swapdex run clwork ")`: a trailing space matches
    // "... clwork --tool claude-code" too, so that form passes while the flag
    // is being printed. Assert the flag's absence directly.
    assert!(
        !said.contains("swapdex run clwork --tool"),
        "Claude is run's default; spelling it out is noise:\n{said}"
    );
}

/// `pause` names two commands as still working. One of them did not.
///
/// `use` resolves the tool from the name by itself - `swapdex use cxwork`
/// answers "default codex account -> cxwork". `serve` does not: it defaults to
/// Claude and looks the name up in Claude's registry alone, so for a Codex-only
/// account the command `pause` had just recommended answers "no account named
/// 'cxwork'". Measured, both of them.
#[test]
fn the_pause_note_names_a_serve_that_works_for_this_account() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "cxwork", "cx@example.com");

    let (out, err, code) = run(root, &["pause", "cxwork"]);
    assert_eq!(code, 0, "pause failed: {err}");
    let said = format!("{out}{err}");
    assert!(
        said.contains("swapdex serve cxwork"),
        "the note no longer names `serve`, so this measures nothing:\n{said}"
    );
    assert!(
        said.contains("swapdex serve cxwork --tool codex"),
        "pause recommends a `serve` that cannot find this account:\n{said}"
    );
}

/// The over-correction: Claude is the default for both commands, so flagging it
/// would add noise to the common case and teach the flag where it is not
/// needed. `use` never needs one at all - it resolves the tool itself.
#[test]
fn the_pause_note_leaves_claudes_default_unspelled() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");

    let (out, err, code) = run(root, &["pause", "work"]);
    assert_eq!(code, 0, "pause failed: {err}");
    let said = format!("{out}{err}");
    assert!(
        said.contains("swapdex serve work"),
        "the note no longer names `serve`:\n{said}"
    );
    assert!(
        !said.contains("--tool"),
        "Claude is the default for both commands; spelling it out is noise:\n{said}"
    );
}

/// `run` defaults to Claude, so advice naming it must carry the tool.
///
/// `sign_in_remedy` exists because of exactly this: "a Codex account that could
/// not serve was told to run `swapdex run codex-test`, and `run` defaults to
/// Claude - so following the instruction launched Claude for a Codex profile."
/// The tip printed after a switch never got that fix, and it is on the common
/// path - every first switch of a tool whose shim is not installed yet.
/// Following it creates a second account of the same name under Claude.
#[test]
fn the_switch_tip_names_the_tool_it_just_switched() {
    let t = fixture();
    let root = t.path();
    seed_codex_slot(root, "work", "work@example.com");

    let (out, err, code) = run(root, &["use", "work", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");
    let said = format!("{out}{err}");
    assert!(
        said.contains("swapdex run work"),
        "the tip no longer names `run`, so this test measures nothing:\n{said}"
    );
    assert!(
        said.contains("swapdex run work --tool codex"),
        "the tip after a Codex switch names a command that runs Claude:\n{said}"
    );
}

/// Gemini and Antigravity have no shim and no `run`: `shim` installs only the
/// Claude and Codex ones, and `run` refuses them outright ("no per-account home
/// to launch into"). Offering both after a Gemini switch promised a shim that
/// is never installed and a command that cannot succeed - and the form it
/// printed, without a `--tool`, quietly built a Claude account of the same
/// name.
#[test]
fn switching_a_tool_with_no_shim_does_not_offer_one() {
    let t = fixture();
    let root = t.path();
    seed_gemini_slot(root, "gwork", "g@example.com");

    let (out, err, code) = run(root, &["use", "gwork", "--tool", "gemini"]);
    assert_eq!(code, 0, "use failed: {err}");
    let said = format!("{out}{err}");
    assert!(
        said.contains("gwork"),
        "the switch printed nothing, so this measures nothing:\n{said}"
    );
    assert!(
        !said.contains("swapdex shim"),
        "promises a gemini shim that `swapdex shim` never installs:\n{said}"
    );
    assert!(
        !said.contains("swapdex run gwork"),
        "names `run`, which refuses gemini and builds a Claude account instead:\n{said}"
    );
}

/// `serve` decides who PAYS for turns, and turns are paid through the proxy.
///
/// There is no Gemini or Antigravity relay: `proxy` and `service install` both
/// refuse those tools by name, with a sentence written for it. `serve` did not
/// ask, so it wrote a serving pointer for a relay that does not exist, reported
/// "now <name>", and told the reader to run `swapdex proxy` - which then
/// refuses to start. Three commands, two answers.
#[test]
fn serve_refuses_a_tool_the_proxy_cannot_carry() {
    let t = fixture();
    let root = t.path();
    seed_gemini_slot(root, "gwork", "g@example.com");

    // The sibling command refuses; without that this test measures nothing.
    let (pout, perr, _) = run(root, &["proxy", "--tool", "gemini"]);
    assert!(
        format!("{pout}{perr}").contains("no gemini relay"),
        "`proxy` no longer refuses gemini, so serve has nothing to disagree with"
    );

    let (out, err2, code2) = run(root, &["serve", "gwork", "--tool", "gemini"]);
    let said = format!("{out}{err2}");
    assert_ne!(
        code2, 0,
        "serve accepted a tool the proxy cannot carry:\n{said}"
    );
    assert!(
        !root.join(".local/share/swapdex/serving-gemini").exists(),
        "serve wrote a serving pointer for a relay that does not exist"
    );
}

/// The line printed after a switch must not send the reader at a command that
/// refuses to run. It offered `swapdex proxy` for every tool, so switching a
/// Gemini account pointed at a relay that does not exist - and without a
/// `--tool`, the command it named starts Claude's.
#[test]
fn switching_a_tool_without_a_relay_does_not_offer_the_proxy() {
    let t = fixture();
    let root = t.path();
    seed_gemini_slot(root, "gwork", "g@example.com");

    let (out, err2, code2) = run(root, &["use", "gwork", "--tool", "gemini"]);
    assert_eq!(code2, 0, "use failed: {err2}");
    let said = format!("{out}{err2}");
    assert!(
        said.contains("gwork"),
        "the switch printed nothing about the account, so this measures nothing:\n{said}"
    );
    assert!(
        !said.contains("swapdex proxy"),
        "switching a gemini account points at a proxy that refuses to start:\n{said}"
    );
}

/// Per-slot login health is checked for one tool out of four.
///
/// `snapshot_refreshed_at` exists because each reader used to work out a
/// tool's freshness for itself and they disagreed; snapshots got one helper
/// covering all four tools. The per-slot walk never did - it calls
/// `claude::slot_login`, so a Codex slot that has never been signed in gets no
/// `slot:` row at all. Slots are the shape `run` and `migrate` steer people
/// into, so this is the common machine, and an absent check reads as a passed
/// one.
#[test]
fn doctor_checks_a_codex_slot_login_the_way_it_checks_a_claude_one() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "cl", "cl@example.com");
    seed_codex_slot(root, "cx", "cx@example.com");
    // Neither account has signed in yet.
    std::fs::remove_file(root.join(".local/share/swapdex/slots/cl/.credentials.json")).unwrap();
    std::fs::remove_file(root.join(".local/share/swapdex/slots/cx/auth.json")).unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("slot:cl"),
        "the Claude half of the comparison is gone, so this test measures nothing:\n{said}"
    );
    assert!(
        said.contains("slot:cx"),
        "doctor reports a Claude slot with no login and says nothing about a Codex one:\n{said}"
    );
}

/// The default pointer is the one pointer nothing validates.
///
/// `serving_dir()` refuses to answer with a directory the registry does not
/// hold - slots.rs tests that. `default_dir()` hands back whatever the file
/// says, so a pointer left naming a slot the registry no longer lists makes
/// `doctor` print `default ok - plain `claude` -> '(unknown)'`: the code has
/// already failed to resolve the name and still records the row as healthy,
/// while the shim exports that directory to CLAUDE_CONFIG_DIR unchecked. The
/// remedy doctor itself gives for a damaged registry - restore slots.json from
/// a backup - is one way to arrive here.
#[test]
fn doctor_reports_a_default_pointer_naming_no_account() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let ptr = root.join(".local/share/swapdex/active-claude");
    assert!(ptr.exists(), "use wrote no default pointer at {ptr:?}");
    let orphan = root.join(".local/share/swapdex/slots/not-in-the-registry");
    std::fs::write(&ptr, orphan.to_string_lossy().as_bytes()).unwrap();

    let (out, err2, code2) = run(root, &["doctor"]);
    let said = format!("{out}{err2}");
    assert!(
        !said.contains("(unknown)"),
        "doctor prints an unresolvable default as if it were an account name:\n{said}"
    );
    assert_ne!(
        code2, 0,
        "doctor exited 0 with a default naming no account:\n{said}"
    );
}

/// The per-tool row was read from the tool's own config dir, so on a machine
/// whose accounts live only as slots `doctor` printed `claude-code ok - not
/// logged in` and, four rows down, `default ok - plain claude -> 'work'`. Both
/// lines are in one screen whose whole job is to tell somebody whether their
/// setup is sound, and they said opposite things about the same machine.
#[test]
fn doctor_agrees_with_the_default_it_reports() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (_, err, code) = run(root, &["use", "work", "--tool", "claude"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, _, _) = run(root, &["doctor"]);
    assert!(
        out.contains("-> 'work'"),
        "doctor does not report the default it is being checked against:\n{out}"
    );
    let row = out
        .lines()
        .find(|l| l.starts_with("claude-code"))
        .unwrap_or_else(|| panic!("doctor has no claude-code row:\n{out}"));
    assert!(
        !row.contains("not logged in"),
        "doctor calls the tool logged out while naming its default account:\n{out}"
    );
    assert!(
        row.contains("work"),
        "doctor's claude-code row does not name the active account: {row:?}"
    );
}

/// Both branches of doctor's default row must name the tool they are about.
///
/// The positive branch says ``plain `claude` -> 'work'`` and the negative one
/// said "no default account set" with no tool in it, so on a machine with a
/// Codex default and no Claude one `doctor` denied having a default two rows
/// below reporting one. One `match`, one scoped arm and one unscoped: the
/// unscoped arm reads as a statement about the machine.
#[test]
fn doctor_scopes_the_missing_default_to_its_tool() {
    let t = fixture();
    let root = t.path();
    // A Claude slot with no default pointer, so the row is reached and takes
    // its negative arm.
    seed_slot(root, "work", "work@example.com");
    // ... while Codex demonstrably has one.
    seed_codex_slot(root, "cx-one", "one@example.com");
    let (_, err, code) = run(root, &["use", "cx-one", "--tool", "codex"]);
    assert_eq!(code, 0, "use failed: {err}");

    let (out, _, _) = run(root, &["doctor"]);
    let row = out
        .lines()
        .find(|l| l.starts_with("default "))
        .unwrap_or_else(|| panic!("doctor has no default row:\n{out}"));
    assert!(
        row.contains("no default"),
        "this machine should have no CLAUDE default: {row:?}"
    );
    assert!(
        row.contains("claude"),
        "the missing-default row does not say which tool it is about: {row:?}"
    );
}

/// The help must name the slash command the installer actually creates.
///
/// It promised `/sx` while `swapdex slash` wrote `~/.claude/commands/swap.md`,
/// which Claude Code exposes as `/swap` - the filename IS the command name. So
/// the one line telling somebody what to type named something that has never
/// existed, and the installer's own output, the closing hint and the Codex
/// skill all said `/swap`. Four places agreed and the help did not.
#[test]
fn the_help_names_the_slash_command_that_gets_installed() {
    let t = fixture();
    let root = t.path();
    let (out, err, code) = run(root, &["slash", "--help"]);
    assert_eq!(code, 0, "slash --help failed:\n{out}{err}");
    assert!(
        out.contains("/swap"),
        "the help does not name the command the installer writes:\n{out}"
    );
    assert!(
        !out.contains("/sx"),
        "the help still names a command nothing installs:\n{out}"
    );
}

/// Every command the binary has must appear in the reference that claims to
/// list them all.
///
/// The README sends people to `docs/COMMANDS.md` for the "full command,
/// exit-code, and environment reference". It listed twenty-five of forty -
/// missing `serve`, which the README itself features; `refresh` and `service`,
/// which are how an account is kept from expiring; `export`/`import`; and the
/// four commands that configure auto-continue. A reference is only a reference
/// while it is complete, and nothing was checking.
#[test]
fn the_command_reference_lists_every_command() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let doc = std::fs::read_to_string(root.join("docs/COMMANDS.md")).expect("read COMMANDS.md");

    let t = fixture();
    let (help, err, code) = run(t.path(), &["--help"]);
    assert_eq!(code, 0, "--help failed:\n{help}{err}");

    let mut missing: Vec<String> = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.starts_with("Options:") {
            break;
        }
        let Some(name) = line
            .strip_prefix("  ")
            .and_then(|l| l.split_whitespace().next())
        else {
            continue;
        };
        // `help` is clap's own; it is not a swapdex command to document.
        if name == "help" || !name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            continue;
        }
        if !doc.contains(&format!("`swapdex {name}")) {
            missing.push(name.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "docs/COMMANDS.md is the reference and does not list: {}",
        missing.join(", ")
    );
}

/// What `ls` lists, `export` must carry.
///
/// `export` walked the slot registries only. On a machine whose accounts are
/// saved profiles, which `ls`, `use` and `rm` all still serve, it wrote an
/// empty manifest and reported success, so the setup restored on the next
/// machine was nothing at all. Its own comment says the one thing an export
/// must never do is hand back a manifest that looks complete and is not; it
/// guarded the registry it could not READ and not the one it never ASKED.
#[test]
fn export_carries_the_accounts_the_listing_shows() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-w", "w@example.com");
    let (o, e, c) = run(root, &["add", "work"]);
    assert_eq!(c, 0, "add failed:\n{o}{e}");
    seed_slot(root, "slotted", "s@example.com");

    let (listed, _, _) = run(root, &["ls"]);
    for name in ["work", "slotted"] {
        assert!(listed.contains(name), "ls does not show {name}:\n{listed}");
    }

    let out = root.join("setup.json");
    let (o, e, c) = run(root, &["export", out.to_str().unwrap()]);
    assert_eq!(c, 0, "export failed:\n{o}{e}");
    let text = std::fs::read_to_string(&out).expect("read the export");
    let v: serde_json::Value = serde_json::from_str(&text).expect("export is json");
    let names: Vec<&str> = v["accounts"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect())
        .unwrap_or_default();
    for name in ["work", "slotted"] {
        assert!(
            names.contains(&name),
            "the export drops an account the listing shows: {name} not in {names:?}"
        );
    }
}

/// Import must leave an account already present as a snapshot alone.
///
/// Export writes both registries, but import compared the manifest with slots
/// alone. Re-importing a snapshot account therefore created a second, empty
/// slot under the same name instead of preserving the local login it found.
#[test]
fn import_recognises_a_snapshot_account_already_here() {
    let t = fixture();
    let root = t.path();
    seed_claude(root, "uuid-w", "w@example.com");
    let (out, err, code) = run(root, &["add", "work", "--tool", "claude-code"]);
    assert_eq!(code, 0, "add failed:\n{out}{err}");

    let manifest = root.join("setup.json");
    std::fs::write(
        &manifest,
        br#"{"version":1,"accounts":[{"name":"work","tool":"claude-code"}]}"#,
    )
    .unwrap();
    let (out, err, code) = run(root, &["import", manifest.to_str().unwrap()]);
    assert_eq!(code, 0, "import failed:\n{out}{err}");
    assert!(
        out.contains("every account in that file is already here"),
        "import did not recognise the snapshot account:\n{out}{err}"
    );
    assert!(
        !out.contains("created work"),
        "import created an empty slot over an account already here:\n{out}{err}"
    );
}

/// The rotation check covers Claude, not only Codex.
///
/// It was written for Codex because Codex keeps the account and the refresh
/// token in one readable blob, and Claude splits them across two parts of the
/// same snapshot - a reason to write more code, not a reason to leave the tool
/// unguarded. Claude's refresh tokens rotate too; the module note that opens
/// refresh.rs is about exactly that.
#[test]
fn use_refuses_a_claude_snapshot_whose_refresh_token_was_rotated_away() {
    let t = fixture();
    let root = t.path();
    // A live slot holding the account with the token the last rotation minted.
    let slot = root.join(".local/share/swapdex/slots/live");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"AT","refreshToken":"RT-NEW","expiresAt":9999999999999,"subscriptionType":"max"}}"#,
    )
    .unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        br#"{"oauthAccount":{"accountUuid":"u-shared","emailAddress":"shared@example.com"}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join(".local/share/swapdex/slots.json"),
        serde_json::to_vec(&serde_json::json!([{
            "name": "live", "id": "live", "config_dir": slot,
            "adopted": false, "tool": "claude-code"}]))
        .unwrap(),
    )
    .unwrap();
    // A saved profile for the SAME account carrying the retired token.
    let snap = root.join(".local/share/swapdex/accounts/stale/claude-code");
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::write(
        snap.join("credentials"),
        br#"{"claudeAiOauth":{"accessToken":"AT-OLD","refreshToken":"RT-OLD"}}"#,
    )
    .unwrap();
    std::fs::write(
        snap.join("oauth_account"),
        br#"{"accountUuid":"u-shared","emailAddress":"shared@example.com"}"#,
    )
    .unwrap();

    let (out, err, code) = run(root, &["use", "stale", "--tool", "claude"]);
    let said = format!("{out}{err}");
    assert_ne!(code, 0, "a retired Claude snapshot was applied:\n{said}");
    assert!(
        said.contains("retired") || said.contains("rotated"),
        "nothing said the token was retired elsewhere:\n{said}"
    );
}

/// A settings file that will not parse must be reported, not absorbed.
///
/// `settings::load` falls back to defaults on a parse error, because every
/// caller wants a value rather than an error. The consequence is that a corrupt
/// file reads as NO settings: every account the user paused is silently back in
/// the proxy's rotation, the strategy is back to default, and the listing shows
/// nothing unusual. The proxy reads the same file, so an account somebody took
/// out of rotation starts paying for turns again with no signal anywhere.
/// `doctor` is the one screen whose job is to find exactly this.
#[test]
fn doctor_reports_a_settings_file_that_will_not_parse() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let (o, e, c) = run(root, &["pause", "work"]);
    assert_eq!(c, 0, "pause failed:\n{o}{e}");

    let settings = root.join(".local/share/swapdex/settings.json");
    assert!(settings.exists(), "pause wrote no settings file");
    std::fs::write(&settings, b"NOT JSON {{{").unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.to_lowercase().contains("settings"),
        "doctor says nothing about a settings file it cannot read:\n{said}"
    );
}

/// The permission check must look where the credentials actually live.
///
/// It walked the four bare paths - `~/.claude/.credentials.json` and its
/// siblings - and never a slot. Under the slot model, which is what `run`,
/// `migrate` and `onboard` all steer people into, every credential lives in a
/// slot instead, so on such a machine the check ran over nothing at all while
/// reporting a clean bill. The README's promise that every credential swapdex
/// writes is 0600 was being verified for the one file it no longer writes.
#[test]
fn doctor_checks_the_permissions_of_credentials_in_slots() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let cred = root
        .join(".local/share/swapdex/slots/work")
        .join(".credentials.json");
    assert!(cred.exists(), "the fixture wrote no slot credential");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cred, std::fs::Permissions::from_mode(0o644)).unwrap();

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("group/world-readable"),
        "doctor passed a world-readable slot credential:\n{said}"
    );
}

/// The over-correction of the test above: a registry that parses must not be
/// announced as damaged. A health check that cries wolf on a healthy machine
/// teaches the user to skim past the row that will one day be real.
#[test]
fn doctor_stays_quiet_about_a_registry_that_parses() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let reg = root.join(".local/share/swapdex/slots.json");
    assert!(reg.exists(), "the fixture wrote no registry");

    let (out, err, _) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        !said.contains("will not parse"),
        "doctor called a readable registry damaged:\n{said}"
    );
}

/// A registry that will not parse is a finding, not an absence.
///
/// `doctor` opened the slot registry with `if let Ok(..)` and skipped every
/// check inside when it would not parse - the account count, the default
/// pointer, the per-slot login rows, all of it. The rows did not say "cannot
/// check"; they simply were not printed, and a shorter clean report reads as a
/// healthier one. So with a damaged registry `doctor` said "everything looks
/// healthy" and exited 0, while `ls` printed a paragraph about the same file
/// being damaged and told the reader how to restore it.
#[test]
fn doctor_reports_a_slot_registry_that_will_not_parse() {
    let t = fixture();
    let root = t.path();
    seed_slot(root, "work", "work@example.com");
    let reg = root.join(".local/share/swapdex/slots.json");
    assert!(reg.exists(), "the fixture wrote no registry");
    std::fs::write(&reg, b"").unwrap();

    // `ls` knows. That is the bar doctor has to meet.
    let (listed, lerr, _) = run(root, &["ls"]);
    assert!(
        format!("{listed}{lerr}").contains("could not be read"),
        "ls no longer reports a damaged registry, so this test is measuring the wrong thing"
    );

    let (out, err, code) = run(root, &["doctor"]);
    let said = format!("{out}{err}");
    assert!(
        said.contains("registry") || said.contains("slots.json"),
        "doctor called a damaged account registry healthy:\n{said}"
    );
    assert_ne!(code, 0, "doctor exited 0 with a damaged registry:\n{said}");
}
