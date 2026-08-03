//! Renewing a lapsed access token, end to end.
//!
//! The failure paths are covered by unit tests and were seen against the real
//! endpoint; the SUCCESS path had never been executed, because it needs an
//! account that is idle AND holds a refresh token the server still honours, and
//! there was not one to spare. This runs the whole path against a token endpoint
//! of our own: the request swapdex builds, the answer it merges, and the
//! credential it writes back.

use std::process::Command;
use std::sync::{Arc, Mutex};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

/// A token endpoint that records what it was asked and answers with new tokens.
fn fake_oauth(sink: Arc<Mutex<Vec<String>>>, response: &'static str) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut rq in server.incoming_requests() {
            let mut body = String::new();
            std::io::Read::read_to_string(rq.as_reader(), &mut body).ok();
            sink.lock().unwrap().push(body);
            let _ = rq.respond(tiny_http::Response::from_string(response));
        }
    });
    format!("http://127.0.0.1:{port}/v1/oauth/token")
}

/// A Claude account whose access token lapsed an hour ago and whose refresh
/// token is still good - the state every idle account reaches.
fn seed_lapsed_account(root: &std::path::Path, name: &str, id: &str) -> std::path::PathBuf {
    let store = root.join(".local/share/swapdex");
    let slot = store.join("slots").join(id);
    std::fs::create_dir_all(&slot).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::fs::write(
        slot.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"OLD-AT","refreshToken":"OLD-RT",
               "expiresAt":{},"refreshTokenExpiresAt":{},"subscriptionType":"max",
               "scopes":["user:inference"]}},"mcpOAuth":{{"keep":"me"}}}}"#,
            now_ms - 3_600_000,
            now_ms + 30 * 86_400_000
        ),
    )
    .unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        format!(r#"{{"oauthAccount":{{"accountUuid":"u-{name}","emailAddress":"{name}@x.com"}}}}"#),
    )
    .unwrap();
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec(&serde_json::json!([{
            "name": name, "id": id, "config_dir": slot, "adopted": false,
            "tool": "claude-code"
        }]))
        .unwrap(),
    )
    .unwrap();
    slot
}

fn credential(slot: &std::path::Path) -> serde_json::Value {
    let bytes = std::fs::read(slot.join(".credentials.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn a_lapsed_account_is_renewed_in_place() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_lapsed_account(root.path(), "work", "aaaa1111");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let url = fake_oauth(
        asked.clone(),
        r#"{"access_token":"NEW-AT","refresh_token":"NEW-RT","expires_in":3600}"#,
    );

    let out = Command::new(bin())
        .args(["refresh", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_OAUTH_URL", &url)
        .env("HOME", root.path())
        .output()
        .unwrap();
    let said =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("work renewed"), "reported success: {said}");

    // What swapdex asked for.
    let body = asked.lock().unwrap().first().cloned().expect("one request");
    let req: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(req["grant_type"], "refresh_token");
    assert_eq!(req["refresh_token"], "OLD-RT", "the token it held");
    assert!(req["client_id"].is_string(), "the client is named: {body}");

    // What it wrote back. The new refresh token REPLACES the old one: the server
    // retires what it just spent, and keeping it would leave the account unable
    // to renew the time after this.
    let c = credential(&slot);
    let o = &c["claudeAiOauth"];
    assert_eq!(o["accessToken"], "NEW-AT");
    assert_eq!(o["refreshToken"], "NEW-RT");
    assert_eq!(o["subscriptionType"], "max", "untouched fields survive");
    assert_eq!(
        c["mcpOAuth"]["keep"], "me",
        "and so does the rest of the file"
    );

    // The expiry is an absolute moment in the future, not the seconds it was sent.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let exp = o["expiresAt"].as_i64().expect("an expiry");
    assert!(
        exp > now_ms && exp <= now_ms + 3_600_000,
        "an hour ahead, not 3600: {exp}"
    );

    // And the account now reads as current rather than expired.
    let after = Command::new(bin())
        .args(["refresh", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_OAUTH_URL", &url)
        .env("HOME", root.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("already current"),
        "a renewed account is not renewed again: {:?}",
        String::from_utf8_lossy(&after.stdout)
    );
    assert_eq!(
        asked.lock().unwrap().len(),
        1,
        "and no second request was made"
    );
}

// A server that renews the access token without issuing a new refresh token:
// dropping the old one would leave the account unable to renew ever again.
#[test]
fn a_renewal_without_a_new_refresh_token_keeps_the_old_one() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_lapsed_account(root.path(), "work", "bbbb2222");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let url = fake_oauth(
        asked.clone(),
        r#"{"access_token":"NEW-AT","expires_in":3600}"#,
    );

    let out = Command::new(bin())
        .args(["refresh", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_OAUTH_URL", &url)
        .env("HOME", root.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("renewed"));
    let o = credential(&slot);
    assert_eq!(o["claudeAiOauth"]["accessToken"], "NEW-AT");
    assert_eq!(o["claudeAiOauth"]["refreshToken"], "OLD-RT");
}

// A refusal must leave the credential exactly as it was: half-writing one is how
// an account becomes unusable without anybody touching it.
#[test]
fn a_refused_renewal_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let slot = seed_lapsed_account(root.path(), "work", "cccc3333");
    let before = std::fs::read(slot.join(".credentials.json")).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let url = fake_oauth(asked.clone(), r#"{"error":"invalid_grant"}"#);

    let out = Command::new(bin())
        .args(["refresh", "work"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_OAUTH_URL", &url)
        .env("HOME", root.path())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        said.contains("could not be renewed"),
        "the refusal is reported: {said}"
    );
    assert_eq!(
        std::fs::read(slot.join(".credentials.json")).unwrap(),
        before,
        "the credential is untouched"
    );
}
