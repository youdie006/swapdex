use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

/// What the fake upstream saw for one request.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    auth: String,
    /// `metadata.user_id` from the body, when the body carried one.
    user_id: Option<String>,
}

/// A fake upstream API: records the Authorization header and the body's account
/// identity, then answers with a small body. No test ever reaches the real API.
fn fake_upstream(sink: Arc<Mutex<Vec<Seen>>>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut rq in server.incoming_requests() {
            let auth = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = Vec::new();
            rq.as_reader().read_to_end(&mut body).ok();
            let user_id = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v["metadata"]["user_id"]
                        .as_str()
                        .map(std::string::ToString::to_string)
                });
            sink.lock().unwrap().push(Seen { auth, user_id });
            let _ = rq.respond(tiny_http::Response::from_string("{\"ok\":true}"));
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// The Authorization values the upstream saw, in order.
fn auths(sink: &Arc<Mutex<Vec<Seen>>>) -> Vec<String> {
    sink.lock()
        .unwrap()
        .iter()
        .map(|s| s.auth.clone())
        .collect()
}

/// Write a slot with a known token and make it the default account.
fn seed_slot(root: &std::path::Path, name: &str, id: &str, token: &str, make_default: bool) {
    let store = root.join(".local/share/swapdex");
    let slot = store.join("slots").join(id);
    std::fs::create_dir_all(&slot).unwrap();
    // The slot's own connected identity, as Claude records it after a login.
    std::fs::write(
        slot.join(".claude.json"),
        format!(
            r#"{{"oauthAccount":{{"accountUuid":"uuid-of-{name}","emailAddress":"{name}@x.com"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        slot.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"R","expiresAt":9999999999999}}}}"#
        ),
    )
    .unwrap();
    let mut recs: Vec<serde_json::Value> = std::fs::read(store.join("slots.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    recs.push(serde_json::json!({
        "name": name, "id": id, "config_dir": slot, "adopted": false
    }));
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&recs).unwrap(),
    )
    .unwrap();
    if make_default {
        std::fs::write(
            store.join("active-claude"),
            slot.to_string_lossy().as_bytes(),
        )
        .unwrap();
    }
}

/// Start `swapdex proxy --port 0` and return (child, port) once it announces.
fn start_proxy(
    root: &std::path::Path,
    upstream: &str,
    extra: &[&str],
) -> (std::process::Child, u16) {
    let mut args = vec!["proxy", "--port", "0"];
    args.extend_from_slice(extra);
    let mut child = Command::new(bin())
        .args(&args)
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_UPSTREAM", upstream)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.stdout.as_mut().unwrap();
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while out.read(&mut b).unwrap_or(0) == 1 {
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
    }
    let line = String::from_utf8_lossy(&line).to_string();
    let port = line
        .rsplit(':')
        .next()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or_else(|| panic!("proxy did not announce a port: {line}"));
    (child, port)
}

/// Post a turn through the proxy and read the body. Non-2xx is a normal answer
/// here (a real client sees the upstream's status verbatim), so the agent must not
/// treat it as an error.
fn post_through(port: u16, body: &str) -> String {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("authorization", "Bearer CLIENT-TOKEN")
        .header("content-type", "application/json")
        .send(body.as_bytes())
        .expect("proxy answered");
    let mut out = String::new();
    resp.body_mut()
        .as_reader()
        .read_to_string(&mut out)
        .unwrap();
    out
}

/// Repoint the default account, the way `swapdex use <name>` does.
fn point_default_at(root: &std::path::Path, id: &str) {
    let store = root.join(".local/share/swapdex");
    let slot = store.join("slots").join(id);
    std::fs::write(
        store.join("active-claude"),
        slot.to_string_lossy().as_bytes(),
    )
    .unwrap();
}

/// The whole point of proxy mode: a conversation that is ALREADY running moves to
/// another account when the pointer changes. No restart, no resume - the next
/// turn simply carries the other account's token.
#[test]
fn a_running_session_follows_a_pointer_change_to_another_account() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());

    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);
    post_through(port, "{\"turn\":1}");
    // Mid-conversation: the user switches accounts.
    point_default_at(root.path(), "bbbb2222");
    post_through(port, "{\"turn\":2}");
    child.kill().ok();

    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-RND".to_string(), "Bearer AT-BSGONG".to_string()],
        "the second turn of the same session was served by the newly chosen account"
    );
}

#[test]
fn proxy_injects_the_slots_token_and_streams_the_response_back() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "work", "aaaa1111", "AT-SLOT", true);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());

    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);
    let body = post_through(port, "{\"model\":\"x\"}");
    child.kill().ok();

    assert!(
        body.contains("\"ok\":true"),
        "response streamed back: {body}"
    );
    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-SLOT".to_string()],
        "the slot's token replaced the client's"
    );
}

/// After a switch the client still names the account the conversation started
/// with; the forwarded body must name the account whose token is serving it, or
/// the request contradicts itself.
#[test]
fn the_forwarded_body_names_the_account_actually_serving_the_turn() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());

    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);
    // The client's body carries rnd's identity, the way Claude wrote it.
    let turn = r#"{"model":"m","metadata":{"user_id":"{\"account_uuid\":\"uuid-of-rnd\"}"}}"#;
    post_through(port, turn);
    point_default_at(root.path(), "bbbb2222");
    post_through(port, turn);
    child.kill().ok();

    let seen = sink.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "two turns reached the upstream");
    assert!(
        seen[0].user_id.as_deref().unwrap().contains("uuid-of-rnd"),
        "turn 1 served by rnd keeps rnd's identity: {:?}",
        seen[0]
    );
    assert!(
        seen[1]
            .user_id
            .as_deref()
            .unwrap()
            .contains("uuid-of-bsgong")
            && !seen[1].user_id.as_deref().unwrap().contains("uuid-of-rnd"),
        "turn 2 served by bsgong carries bsgong's identity: {:?}",
        seen[1]
    );
}

/// A fake upstream whose FIRST answer reports the account spent, then answers
/// normally - the shape of hitting a limit mid-conversation.
fn fake_upstream_spent_once(sink: Arc<Mutex<Vec<Seen>>>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        let mut first = true;
        for mut rq in server.incoming_requests() {
            let auth = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = Vec::new();
            rq.as_reader().read_to_end(&mut body).ok();
            sink.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            let status = if first { "rejected" } else { "allowed" };
            first = false;
            let resp = tiny_http::Response::from_string("{\"ok\":true}").with_header(
                tiny_http::Header::from_bytes(
                    &b"anthropic-ratelimit-unified-status"[..],
                    status.as_bytes(),
                )
                .unwrap(),
            );
            let _ = rq.respond(resp);
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// --auto: when a turn comes back marked spent, the NEXT turn of the same session
/// continues on another account by itself. The spent turn still reaches the
/// client intact - rotation happens at the boundary, never mid-answer.
#[test]
fn auto_continues_the_session_on_another_account_when_one_is_spent() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream_spent_once(sink.clone());

    let (mut child, port) = start_proxy(root.path(), &upstream, &["--auto"]);
    let first = post_through(port, "{\"turn\":1}");
    let second = post_through(port, "{\"turn\":2}");
    child.kill().ok();

    assert!(
        first.contains("\"ok\":true"),
        "the spent turn still reached the client: {first}"
    );
    assert!(
        second.contains("\"ok\":true"),
        "second turn served: {second}"
    );
    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-RND".to_string(), "Bearer AT-BSGONG".to_string()],
        "the session continued on the other account with no user action"
    );
}

/// Without --auto nothing rotates: a spent account keeps serving (and failing),
/// because moving accounts by itself is opt-in.
#[test]
fn without_auto_a_spent_account_is_not_rotated_away_from() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream_spent_once(sink.clone());

    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);
    post_through(port, "{\"turn\":1}");
    post_through(port, "{\"turn\":2}");
    child.kill().ok();

    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-RND".to_string(), "Bearer AT-RND".to_string()],
        "no rotation without --auto"
    );
}

/// A stale slot login is refused by the API (401). That is not a quota problem,
/// so --auto must move the session on and the reason must be actionable rather
/// than a bare 401.
#[test]
fn auto_moves_on_when_an_accounts_login_is_refused() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    // First answer 401 (stale login), then serve normally.
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port_up = server.server_addr().to_ip().unwrap().port();
    let s2 = sink.clone();
    std::thread::spawn(move || {
        let mut first = true;
        for mut rq in server.incoming_requests() {
            let auth = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut b = Vec::new();
            rq.as_reader().read_to_end(&mut b).ok();
            s2.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            let code = if first { 401 } else { 200 };
            first = false;
            let _ = rq.respond(
                tiny_http::Response::from_string("{}")
                    .with_status_code(tiny_http::StatusCode(code)),
            );
        }
    });

    let (mut child, port) = start_proxy(
        root.path(),
        &format!("http://127.0.0.1:{port_up}"),
        &["--auto"],
    );
    let first = post_through(port, "{\"turn\":1}");
    post_through(port, "{\"turn\":2}");
    child.kill().ok();

    assert!(
        !first.is_empty(),
        "the turn was re-served on another account instead of failing"
    );
    assert_eq!(
        auths(&sink),
        vec![
            "Bearer AT-RND".to_string(),
            "Bearer AT-BSGONG".to_string(),
            "Bearer AT-BSGONG".to_string()
        ],
        "a refused login re-serves the turn elsewhere, and stays out of the way after"
    );
}

/// A throttle 429 (x-should-retry, no unified headers - the real shape) is fixed
/// by waiting and retrying the SAME account, not by abandoning it. The client
/// sees the eventual success, never the throttle.
#[test]
fn a_throttled_turn_is_retried_on_the_same_account() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let sink = Arc::new(Mutex::new(Vec::new()));

    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port_up = server.server_addr().to_ip().unwrap().port();
    let s2 = sink.clone();
    std::thread::spawn(move || {
        let mut first = true;
        for mut rq in server.incoming_requests() {
            let auth = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut b = Vec::new();
            rq.as_reader().read_to_end(&mut b).ok();
            s2.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            let resp = if first {
                first = false;
                tiny_http::Response::from_string("{\"type\":\"error\"}")
                    .with_status_code(tiny_http::StatusCode(429))
                    .with_header(
                        tiny_http::Header::from_bytes(&b"x-should-retry"[..], &b"true"[..])
                            .unwrap(),
                    )
            } else {
                tiny_http::Response::from_string("{\"ok\":true}")
                    .with_status_code(tiny_http::StatusCode(200))
                    .with_header(
                        tiny_http::Header::from_bytes(&b"x-should-retry"[..], &b"false"[..])
                            .unwrap(),
                    )
            };
            let _ = rq.respond(resp);
        }
    });

    let (mut child, port) = start_proxy(
        root.path(),
        &format!("http://127.0.0.1:{port_up}"),
        &["--auto"],
    );
    let body = post_through(port, "{\"turn\":1}");
    child.kill().ok();

    assert!(
        body.contains("\"ok\":true"),
        "the client got the retried success, not the throttle: {body}"
    );
    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-RND".to_string(), "Bearer AT-RND".to_string()],
        "retried on the SAME account - a throttle is not exhaustion"
    );
}

/// A 429 that is NOT a passing throttle (no retry hint) is the wall: --auto must
/// continue the session on another account. Before this, a 429 carried no unified
/// headers, so nothing marked the account spent and the user stayed stuck on it.
#[test]
fn auto_continues_the_session_when_a_turn_is_rate_limited() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let sink = Arc::new(Mutex::new(Vec::new()));

    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port_up = server.server_addr().to_ip().unwrap().port();
    let s2 = sink.clone();
    std::thread::spawn(move || {
        let mut first = true;
        for mut rq in server.incoming_requests() {
            let auth = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut b = Vec::new();
            rq.as_reader().read_to_end(&mut b).ok();
            s2.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            // A hard 429: no x-should-retry, so it is the wall, not a throttle.
            let resp = if first {
                first = false;
                tiny_http::Response::from_string(
                    "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}",
                )
                .with_status_code(tiny_http::StatusCode(429))
            } else {
                tiny_http::Response::from_string("{\"ok\":true}")
            };
            let _ = rq.respond(resp);
        }
    });

    let (mut child, port) = start_proxy(
        root.path(),
        &format!("http://127.0.0.1:{port_up}"),
        &["--auto"],
    );
    let first = post_through(port, "{\"turn\":1}");
    post_through(port, "{\"turn\":2}");
    child.kill().ok();

    assert!(
        first.contains("\"ok\":true"),
        "the client never saw the rate limit - the turn was re-served elsewhere: {first}"
    );
    assert_eq!(
        auths(&sink),
        vec![
            "Bearer AT-RND".to_string(),
            "Bearer AT-BSGONG".to_string(),
            "Bearer AT-BSGONG".to_string()
        ],
        "turn 1 hit the wall on rnd and was immediately re-served by bsgong, \
         which then serves turn 2 as well"
    );
}

/// Rotation must not hand the session to a slot that was never signed into - that
/// just fails the next turn. Here only "rnd" (spent) and "fresh" have logins;
/// "empty" has none, so it must be skipped even though it is listed first.
#[test]
fn rotation_skips_a_slot_that_has_no_login() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    // A slot in the registry with NO credential file at all.
    let store = root.path().join(".local/share/swapdex");
    let empty = store.join("slots").join("cccc3333");
    std::fs::create_dir_all(&empty).unwrap();
    let mut recs: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(store.join("slots.json")).unwrap()).unwrap();
    recs.push(serde_json::json!({
        "name": "empty", "id": "cccc3333", "config_dir": empty, "adopted": false
    }));
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&recs).unwrap(),
    )
    .unwrap();
    seed_slot(root.path(), "fresh", "dddd4444", "AT-FRESH", false);

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream_spent_once(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &["--auto"]);
    post_through(port, "{\"turn\":1}");
    post_through(port, "{\"turn\":2}");
    child.kill().ok();

    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-RND".to_string(), "Bearer AT-FRESH".to_string()],
        "the loginless slot was skipped in favour of one that can actually serve"
    );
}

/// A disabled account is one the user said not to pick automatically, so
/// rotation must skip it - while an explicit priority decides who is reached for
/// first among the rest.
#[test]
fn rotation_skips_disabled_accounts_and_follows_priority() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "skipme", "bbbb2222", "AT-SKIP", false);
    seed_slot(root.path(), "wanted", "cccc3333", "AT-WANTED", false);
    // skipme is out of rotation; wanted is ranked ahead of everything else.
    std::fs::write(
        root.path().join(".local/share/swapdex/settings.json"),
        br#"{"disabled":["skipme"],"priority":["wanted"]}"#,
    )
    .unwrap();

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream_spent_once(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &["--auto"]);
    post_through(port, "{\"turn\":1}");
    post_through(port, "{\"turn\":2}");
    child.kill().ok();

    let seen = auths(&sink);
    assert!(
        !seen.iter().any(|a| a.contains("AT-SKIP")),
        "the disabled account was never picked: {seen:?}"
    );
    assert!(
        seen.contains(&"Bearer AT-WANTED".to_string()),
        "the ranked account was reached for first: {seen:?}"
    );
}

/// A stand-in for curl that answers the usage endpoint: the account whose token
/// matches is reported near its limit, everyone else comfortably below. `quota`
/// shells out and reads the body followed by the status code on the last line,
/// so the fixture matches that shape exactly.
fn fake_curl(root: &std::path::Path, full_token: &str) -> std::path::PathBuf {
    let dir = root.join("fakebin");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("curl");
    std::fs::write(
        &f,
        format!(
            "#!/bin/sh\ncfg=$(cat)\nif echo \"$cfg\" | grep -q '{full_token}'; then\n\
             printf '{{\"five_hour\":{{\"utilization\":0.99}}}}\\n200'\n\
             else\n\
             printf '{{\"five_hour\":{{\"utilization\":0.04}}}}\\n200'\n\
             fi\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    f
}

/// With a threshold set, an account measured at or past it does not get the next
/// turn at all - the conversation steps across BEFORE anything is refused, so no
/// turn is ever spent discovering the wall.
#[test]
fn a_threshold_steps_off_before_the_account_refuses() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "nearly", "aaaa1111", "AT-NEARLY", true);
    seed_slot(root.path(), "fresh", "bbbb2222", "AT-FRESH", false);
    let curl = fake_curl(root.path(), "AT-NEARLY");
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());

    let mut child = Command::new(bin())
        .args(["proxy", "--port", "0", "--auto", "--threshold", "0.98"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_UPSTREAM", &upstream)
        .env("SWAPDEX_CURL", &curl)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let port = {
        let out = child.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while out.read(&mut b).unwrap_or(0) == 1 {
            if b[0] == b'\n' {
                break;
            }
            line.push(b[0]);
        }
        String::from_utf8_lossy(&line)
            .rsplit(':')
            .next()
            .and_then(|p| p.trim().parse::<u16>().ok())
            .expect("port")
    };

    post_through(port, "{\"turn\":1}");
    child.kill().ok();
    child.wait().ok(); // reap it, so the test leaves no zombie behind

    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-FRESH".to_string()],
        "the near-limit account never served a turn: it was stepped over"
    );
}

/// When swapdex has no usable login to offer, it must get out of the way: the
/// turn goes upstream with the CLIENT's own Authorization, which is what Claude
/// would have sent with no proxy at all. Being unable to help is not a reason to
/// break the tool.
#[test]
fn an_unusable_account_falls_back_to_the_clients_own_login() {
    let root = tempfile::tempdir().unwrap();
    // A slot in the registry whose credential is unreadable.
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots").join("aaaa1111");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(slot.join(".credentials.json"), b"not json").unwrap();
    std::fs::write(
        store.join("slots.json"),
        format!(
            r#"[{{"name":"broken","id":"aaaa1111","config_dir":"{}","adopted":false}}]"#,
            slot.display()
        ),
    )
    .unwrap();
    std::fs::write(
        store.join("active-claude"),
        slot.to_string_lossy().as_bytes(),
    )
    .unwrap();

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);
    let body = post_through(port, "{\"turn\":1}");
    child.kill().ok();
    child.wait().ok();

    assert!(
        body.contains("\"ok\":true"),
        "the turn still went through: {body}"
    );
    assert_eq!(
        auths(&sink),
        vec!["Bearer CLIENT-TOKEN".to_string()],
        "the client's own login was forwarded, not a failure"
    );
}

/// An expired slot token is stepped over BEFORE the request goes out: sending it
/// would earn a 401 that nothing here can fix, so the client's own login is used
/// instead and the turn succeeds.
#[test]
fn an_expired_slot_token_never_reaches_upstream() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots").join("aaaa1111");
    std::fs::create_dir_all(&slot).unwrap();
    // Signed in once, long ago: readable, and long past its expiry.
    std::fs::write(
        slot.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"AT-STALE","refreshToken":"R","expiresAt":1}}"#,
    )
    .unwrap();
    std::fs::write(
        store.join("slots.json"),
        format!(
            r#"[{{"name":"lapsed","id":"aaaa1111","config_dir":"{}","adopted":false}}]"#,
            slot.display()
        ),
    )
    .unwrap();
    std::fs::write(
        store.join("active-claude"),
        slot.to_string_lossy().as_bytes(),
    )
    .unwrap();

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);
    let body = post_through(port, "{\"turn\":1}");
    child.kill().ok();
    child.wait().ok();

    assert!(
        body.contains("\"ok\":true"),
        "the turn went through: {body}"
    );
    let seen = auths(&sink);
    assert!(
        !seen.iter().any(|a| a.contains("AT-STALE")),
        "the lapsed token was never sent: {seen:?}"
    );
    assert_eq!(seen, vec!["Bearer CLIENT-TOKEN".to_string()]);
}
