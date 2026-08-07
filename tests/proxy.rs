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

/// The port out of the proxy's announcement line, wherever in it the address
/// sits. A stricter parser meant a reworded first line panicked the test BEFORE
/// it could kill the child, stranding one proxy per test.
fn parse_port(line: &str) -> Option<u16> {
    line.split_whitespace()
        .filter_map(|w| w.rsplit(':').next())
        .find_map(|p| p.trim().parse::<u16>().ok())
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
    let port = parse_port(&line).unwrap_or_else(|| panic!("proxy did not announce a port: {line}"));
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
             printf '{{\"five_hour\":{{\"utilization\":99.0}}}}\\n200'\n\
             else\n\
             printf '{{\"five_hour\":{{\"utilization\":4.0}}}}\\n200'\n\
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

/// Even when every account swapdex manages refuses, the user must still be able
/// to work: the turn falls back to the login the client sent, which is what Claude
/// would have used with no proxy at all.
#[test]
fn a_turn_still_goes_through_when_every_account_is_refused() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "one", "aaaa1111", "AT-ONE", true);
    let sink = Arc::new(Mutex::new(Vec::new()));

    // Upstream refuses the managed token (401) and accepts the client's own.
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port_up = server.server_addr().to_ip().unwrap().port();
    let s2 = sink.clone();
    std::thread::spawn(move || {
        for mut rq in server.incoming_requests() {
            let auth = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut b = Vec::new();
            rq.as_reader().read_to_end(&mut b).ok();
            let refused = auth.contains("AT-ONE");
            s2.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            let resp = if refused {
                tiny_http::Response::from_string("{}").with_status_code(tiny_http::StatusCode(401))
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
    let body = post_through(port, "{\"turn\":1}");
    child.kill().ok();
    child.wait().ok();

    assert!(
        body.contains("\"ok\":true"),
        "the user can still work: {body}"
    );
    assert!(
        auths(&sink).contains(&"Bearer CLIENT-TOKEN".to_string()),
        "it fell back to the client's own login: {:?}",
        auths(&sink)
    );
}

/// Two accounts either side of the threshold must not trade the session back and
/// forth: after a pre-emptive move, the next turns stay put until the cooldown
/// passes. Every hop costs the prompt cache, so a flapping proxy is worse than a
/// slightly full account.
#[test]
fn a_preemptive_move_does_not_flap_between_two_full_accounts() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "first", "aaaa1111", "AT-FIRST", true);
    seed_slot(root.path(), "second", "bbbb2222", "AT-SECOND", false);
    // Both accounts read as near their limit, which is exactly the shape that
    // makes a naive threshold switch oscillate.
    let dir = root.path().join("fakebin");
    std::fs::create_dir_all(&dir).unwrap();
    let curl = dir.join("curl");
    std::fs::write(
        &curl,
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"five_hour\":{\"utilization\":99.0}}\\n200'\n",
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();

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

    for _ in 0..4 {
        post_through(port, "{\"t\":1}");
    }
    child.kill().ok();
    child.wait().ok();

    // Whatever it settled on, it must have stayed there: at most one change of
    // account across four turns.
    let seen = auths(&sink);
    let hops = seen.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(
        hops <= 1,
        "the session should not bounce between accounts, saw {hops} changes: {seen:?}"
    );
}

/// The UI marks the account that is actually taking turns, so the proxy has to
/// record it: after a rotation the pointer and the server differ, and a marker
/// showing the pointer sits on an account that cannot serve.
#[test]
fn the_proxy_records_which_account_is_serving() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "spare", "bbbb2222", "AT-SPARE", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    // The first account is spent; the turn moves to the other one.
    let upstream = fake_upstream_spent_once(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &["--auto"]);

    post_through(port, "{\"t\":1}");
    let serving = root.path().join(".local/share/swapdex/proxy-serving");
    let first = std::fs::read_to_string(&serving).unwrap_or_default();
    assert_eq!(first.trim(), "rnd", "it starts on the pointed-at account");

    post_through(port, "{\"t\":2}");
    let after = std::fs::read_to_string(&serving).unwrap_or_default();
    child.kill().ok();
    child.wait().ok();
    assert_eq!(
        after.trim(),
        "spare",
        "after rotating, the record follows the account actually serving"
    );
}

/// Updating swapdex does not update a proxy that is already running, so a fix can
/// be installed, verified, and still not be what answers the next request. The
/// marker records which build is serving, and --ensure replaces an outdated one on
/// the SAME port - sessions already point there.
#[test]
fn ensure_replaces_a_proxy_from_an_older_build() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "one", "aaaa1111", "AT-ONE", true);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);

    let marker = root.path().join(".local/share/swapdex/proxy");
    let before = std::fs::read_to_string(&marker).unwrap();
    let mut parts = before.split_whitespace();
    let pid: i32 = parts.next().unwrap().parse().unwrap();
    assert_eq!(
        parts.next().unwrap().parse::<u16>().unwrap(),
        port,
        "the marker carries the port"
    );
    assert!(
        parts.next().is_some_and(|b| !b.is_empty()),
        "and which build is serving: {before}"
    );

    // Pretend it is an older build.
    std::fs::write(&marker, format!("{pid} {port} 0.0.0-old\n")).unwrap();
    let out = Command::new(bin())
        .args(["proxy", "--ensure"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_UPSTREAM", &upstream)
        .output()
        .unwrap();
    let printed: u16 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("a port was printed");
    assert_eq!(printed, port, "the replacement keeps the port sessions use");

    let after = std::fs::read_to_string(&marker).unwrap();
    let new_pid: i32 = after.split_whitespace().next().unwrap().parse().unwrap();
    assert_ne!(new_pid, pid, "it is a different process: {after}");
    assert!(
        !after.contains("0.0.0-old"),
        "and the current build: {after}"
    );

    // Clean up whichever proxies are left.
    child.kill().ok();
    child.wait().ok();
    unsafe { libc::kill(new_pid, libc::SIGTERM) };
}

/// A fake Codex backend: records the Authorization and ChatGPT-Account-ID it was
/// given, plus the path, then answers. No test ever reaches the real backend.
fn fake_codex_upstream(sink: Arc<Mutex<Vec<(String, String, String)>>>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut rq in server.incoming_requests() {
            let head = |name: &'static str| {
                rq.headers()
                    .iter()
                    .find(|h| h.field.equiv(name))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default()
            };
            let seen = (
                head("authorization"),
                head("chatgpt-account-id"),
                rq.url().to_string(),
            );
            let mut body = Vec::new();
            rq.as_reader().read_to_end(&mut body).ok();
            sink.lock().unwrap().push(seen);
            let _ = rq.respond(tiny_http::Response::from_string("{\"ok\":true}"));
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Write a Codex slot holding its own ChatGPT login, and optionally make it the
/// default Codex account.
fn seed_codex_slot(
    root: &std::path::Path,
    name: &str,
    id: &str,
    token: &str,
    account_id: &str,
    make_default: bool,
) {
    let store = root.join(".local/share/swapdex");
    let slot = store.join("slots").join(id);
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join("auth.json"),
        format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{token}",
               "refresh_token":"RT","account_id":"{account_id}"}}}}"#
        ),
    )
    .unwrap();
    let mut recs: Vec<serde_json::Value> = std::fs::read(store.join("slots.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    recs.push(serde_json::json!({
        "name": name, "id": id, "config_dir": slot, "adopted": false, "tool": "codex"
    }));
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&recs).unwrap(),
    )
    .unwrap();
    if make_default {
        std::fs::write(
            store.join("active-codex"),
            slot.to_string_lossy().as_bytes(),
        )
        .unwrap();
    }
}

// Codex sends its own OAuth bearer and ChatGPT-Account-ID on every turn, so
// changing accounts mid-conversation is a rewrite of that pair - and it has to be
// BOTH, from the same slot, or the backend refuses the request.
#[test]
fn a_running_codex_session_follows_a_pointer_change() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "cccc1111",
        "AT-WORK",
        "acct-work",
        true,
    );
    seed_codex_slot(
        root.path(),
        "home",
        "dddd2222",
        "AT-HOME",
        "acct-home",
        false,
    );
    let sink: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream(sink.clone());

    let mut child = Command::new(bin())
        .args(["proxy", "--port", "0", "--tool", "codex"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_UPSTREAM_CODEX", &upstream)
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
        let line = String::from_utf8_lossy(&line).to_string();
        line.rsplit(':')
            .next()
            .and_then(|p| p.trim().parse::<u16>().ok())
            .unwrap_or_else(|| panic!("codex proxy did not announce a port: {line}"))
    };

    let post = || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            // What Codex itself sends: its own pair, which swapdex replaces.
            .header("authorization", "Bearer CLIENT-TOKEN")
            .header("chatgpt-account-id", "acct-client")
            .header("content-type", "application/json")
            .send(b"{\"input\":[]}".as_slice())
            .expect("proxy answered");
        let mut out = String::new();
        resp.body_mut()
            .as_reader()
            .read_to_string(&mut out)
            .unwrap();
        out
    };

    post();
    // Mid-conversation, the user switches Codex accounts.
    let store = root.path().join(".local/share/swapdex");
    std::fs::write(
        store.join("active-codex"),
        store
            .join("slots")
            .join("dddd2222")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    post();
    child.kill().ok();
    child.wait().ok();

    let seen = sink.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "both turns reached the backend: {seen:?}");
    assert_eq!(
        (seen[0].0.as_str(), seen[0].1.as_str()),
        ("Bearer AT-WORK", "acct-work"),
        "the first turn used the default account's own pair, not the client's"
    );
    assert_eq!(
        (seen[1].0.as_str(), seen[1].1.as_str()),
        ("Bearer AT-HOME", "acct-home"),
        "the running session moved to the other account, token AND account-id"
    );
    assert!(
        seen[0].2.ends_with("/responses"),
        "forwarded to the backend's responses path: {:?}",
        seen[0].2
    );
}

/// A fake Codex backend that refuses ONE account and serves every other. Records
/// the (authorization, account-id) pair of each request it saw.
fn fake_codex_upstream_refusing(
    sink: Arc<Mutex<Vec<(String, String)>>>,
    refuse_token: &'static str,
    status: u16,
) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut rq in server.incoming_requests() {
            let head = |name: &'static str| {
                rq.headers()
                    .iter()
                    .find(|h| h.field.equiv(name))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default()
            };
            let auth = head("authorization");
            let acct = head("chatgpt-account-id");
            let mut body = Vec::new();
            rq.as_reader().read_to_end(&mut body).ok();
            sink.lock().unwrap().push((auth.clone(), acct));
            let refused = auth.contains(refuse_token);
            let (code, text) = if refused {
                (status, "{\"error\":\"no\"}")
            } else {
                (200, "{\"ok\":true}")
            };
            let _ = rq.respond(
                tiny_http::Response::from_string(text)
                    .with_status_code(tiny_http::StatusCode(code)),
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Start a Codex proxy against `upstream` and return (child, port).
fn start_codex_proxy(
    root: &std::path::Path,
    upstream: &str,
    extra: &[&str],
) -> (std::process::Child, u16) {
    let mut args = vec!["proxy", "--port", "0", "--tool", "codex"];
    args.extend_from_slice(extra);
    let mut child = Command::new(bin())
        .args(&args)
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_UPSTREAM_CODEX", upstream)
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
    let port =
        parse_port(&line).unwrap_or_else(|| panic!("codex proxy did not announce a port: {line}"));
    (child, port)
}

/// Post one Codex turn through the proxy and return (status, body).
fn post_codex_turn(port: u16) -> (u16, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("authorization", "Bearer CLIENT-TOKEN")
        .header("chatgpt-account-id", "acct-client")
        .header("content-type", "application/json")
        .send(b"{\"input\":[]}".as_slice())
        .expect("proxy answered");
    let status = resp.status().as_u16();
    let mut out = String::new();
    resp.body_mut()
        .as_reader()
        .read_to_string(&mut out)
        .unwrap();
    (status, out)
}

// The point of --auto for Codex: a turn the current account cannot serve is
// handed to another one and served THERE, rather than handed back as a failure.
// Codex has no zero-spend usage endpoint to read ahead of the wall, so this
// refusal is the only signal there is - if it is not acted on, nothing is.
#[test]
fn a_refused_codex_turn_is_re_served_on_another_account() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "cccc1111",
        "AT-WORK",
        "acct-work",
        true,
    );
    seed_codex_slot(
        root.path(),
        "home",
        "dddd2222",
        "AT-HOME",
        "acct-home",
        false,
    );
    let sink: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream_refusing(sink.clone(), "AT-WORK", 429);

    let (mut child, port) = start_codex_proxy(root.path(), &upstream, &["--auto"]);
    let (status, body) = post_codex_turn(port);
    child.kill().ok();
    child.wait().ok();

    let seen = sink.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        2,
        "the same turn was tried twice - once refused, once served: {seen:?}"
    );
    assert_eq!(
        (seen[0].0.as_str(), seen[0].1.as_str()),
        ("Bearer AT-WORK", "acct-work"),
        "the default account went first"
    );
    assert_eq!(
        (seen[1].0.as_str(), seen[1].1.as_str()),
        ("Bearer AT-HOME", "acct-home"),
        "and the pair moved together to the account that could serve it"
    );
    assert_eq!(status, 200, "the client got the answer, not the refusal");
    assert!(body.contains("ok"), "body relayed from the serving account");
}

// Without --auto the refusal is the answer. Moving a session on by itself is a
// decision the user opts into, and a proxy that quietly reached for another
// account would spend quota nobody asked it to spend.
#[test]
fn without_auto_a_refused_codex_turn_is_returned_as_is() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "eeee1111",
        "AT-WORK",
        "acct-work",
        true,
    );
    seed_codex_slot(
        root.path(),
        "home",
        "ffff2222",
        "AT-HOME",
        "acct-home",
        false,
    );
    let sink: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream_refusing(sink.clone(), "AT-WORK", 429);

    let (mut child, port) = start_codex_proxy(root.path(), &upstream, &[]);
    let (status, _) = post_codex_turn(port);
    child.kill().ok();
    child.wait().ok();

    assert_eq!(status, 429, "the upstream's answer, verbatim");
    assert_eq!(
        sink.lock().unwrap().len(),
        1,
        "no other account was touched"
    );
}

// A 401 is not a quota problem, but it is equally a turn this account cannot
// serve - so with --auto it moves too, and the refused account is kept out of
// the rest of the run rather than tried again on the next turn.
#[test]
fn a_rejected_codex_login_also_hands_the_turn_on() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "aaaa9999",
        "AT-WORK",
        "acct-work",
        true,
    );
    seed_codex_slot(
        root.path(),
        "home",
        "bbbb9999",
        "AT-HOME",
        "acct-home",
        false,
    );
    let sink: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream_refusing(sink.clone(), "AT-WORK", 401);

    let (mut child, port) = start_codex_proxy(root.path(), &upstream, &["--auto"]);
    let (first, _) = post_codex_turn(port);
    // A second turn must not walk back into the account that just refused.
    let (second, _) = post_codex_turn(port);
    child.kill().ok();
    child.wait().ok();

    assert_eq!((first, second), (200, 200), "both turns were served");
    let tokens: Vec<String> = sink
        .lock()
        .unwrap()
        .iter()
        .map(|(a, _)| a.clone())
        .collect();
    assert_eq!(
        tokens,
        vec![
            "Bearer AT-WORK".to_string(),
            "Bearer AT-HOME".to_string(),
            "Bearer AT-HOME".to_string(),
        ],
        "the rejected account is tried once, then left alone: {tokens:?}"
    );
}

// The goal the tool exists for: conversations stay in one place, accounts swap
// underneath as they run out. `serve` says who pays without moving where
// sessions start, so a running conversation changes account and its store - and
// therefore everything `claude -r` can offer - is untouched.
#[test]
fn serve_moves_who_pays_without_moving_where_sessions_live() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "home", "aaaa1111", "AT-HOME", true);
    seed_slot(root.path(), "payer", "bbbb2222", "AT-PAYER", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);

    post_through(port, "{\"turn\":1}");

    // Hand turns to the other account, the way `swapdex serve payer` does.
    let store = root.path().join(".local/share/swapdex");
    let payer_dir = store.join("slots").join("bbbb2222");
    std::fs::write(
        store.join("serving-claude"),
        payer_dir.to_string_lossy().as_bytes(),
    )
    .unwrap();
    post_through(port, "{\"turn\":2}");
    child.kill().ok();
    child.wait().ok();

    assert_eq!(
        auths(&sink),
        vec!["Bearer AT-HOME".to_string(), "Bearer AT-PAYER".to_string()],
        "the running conversation changed account mid-flight"
    );
    // And where sessions start never moved: that pointer is what decides which
    // conversations exist for `-r`, and nothing touched it.
    let launch = std::fs::read_to_string(store.join("active-claude")).unwrap();
    assert!(
        launch.trim().ends_with("aaaa1111"),
        "the conversation store is untouched: {launch}"
    );
}

// An authentication exchange is between the user and the vendor. swapdex has no
// business rewriting it - and it did: the client's own Authorization was replaced
// with a slot's token, so a sign-in typed INSIDE a running session (where the
// proxy address is already in the environment and no shim guard can see it) came
// back "successful" as whichever account the proxy happened to hold.
#[test]
fn an_authentication_request_passes_through_untouched() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "home", "aaaa1111", "AT-HOME", true);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &[]);

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    for path in ["/v1/oauth/token", "/oauth/authorize", "/v1/oauth/revoke"] {
        let _ = agent
            .post(format!("http://127.0.0.1:{port}{path}"))
            .header("authorization", "Bearer CLIENT-OWN")
            .header("content-type", "application/json")
            .send(b"{\"code\":\"abc\"}".as_slice());
    }
    // A normal turn still gets the slot's token - the exemption is narrow.
    post_through(port, "{\"turn\":1}");
    child.kill().ok();
    child.wait().ok();

    let seen = auths(&sink);
    assert_eq!(seen.len(), 4, "all four reached upstream: {seen:?}");
    assert!(
        seen[..3].iter().all(|a| a == "Bearer CLIENT-OWN"),
        "an auth exchange carries the user's own credential, not a slot's: {seen:?}"
    );
    assert_eq!(
        seen[3], "Bearer AT-HOME",
        "and ordinary traffic is still served by the account: {seen:?}"
    );
}

// Two proxies, two markers. They shared one file, so whichever answered a turn
// last decided what BOTH dashboards read - a Codex account appeared as the one
// serving Claude's turns, and no Claude row matched it.
#[test]
fn each_tools_proxy_records_its_own_serving_account() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "home", "aaaa1111", "AT-HOME", true);
    seed_codex_slot(
        root.path(),
        "work",
        "cccc1111",
        "AT-WORK",
        "acct-work",
        true,
    );

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());
    let (mut claude, cport) = start_proxy(root.path(), &upstream, &[]);

    let csink: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let cupstream = fake_codex_upstream(csink.clone());
    let (mut codex, xport) = start_codex_proxy(root.path(), &cupstream, &[]);

    post_through(cport, "{\"turn\":1}");
    post_codex_turn(xport);
    // Claude again, last: with one shared file the Codex name would have stuck.
    post_through(cport, "{\"turn\":2}");

    let store = root.path().join(".local/share/swapdex");
    let claude_says = std::fs::read_to_string(store.join("proxy-serving")).unwrap();
    let codex_says = std::fs::read_to_string(store.join("proxy-serving-codex")).unwrap();
    claude.kill().ok();
    claude.wait().ok();
    codex.kill().ok();
    codex.wait().ok();

    assert_eq!(claude_says.trim(), "home", "claude's own account");
    assert_eq!(codex_says.trim(), "work", "codex's own, kept apart");
}

// The same arrangement for Codex: conversations stay in one home, accounts swap
// underneath. Without this, changing accounts meant changing CODEX_HOME, which
// is where Codex keeps its transcripts - so every switch split the history, and
// a machine ended up with 256 conversations in one account and 2 in the other.
#[test]
fn serve_moves_who_pays_for_codex_without_moving_its_transcripts() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "home",
        "aaaa1111",
        "AT-HOME",
        "acct-home",
        true,
    );
    seed_codex_slot(
        root.path(),
        "payer",
        "bbbb2222",
        "AT-PAYER",
        "acct-payer",
        false,
    );
    let sink: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream(sink.clone());
    let (mut child, port) = start_codex_proxy(root.path(), &upstream, &[]);

    post_codex_turn(port);
    // Hand turns to the other account, the way `swapdex serve payer --tool codex`
    // does - without touching where sessions start.
    let store = root.path().join(".local/share/swapdex");
    std::fs::write(
        store.join("serving-codex"),
        store
            .join("slots")
            .join("bbbb2222")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    post_codex_turn(port);
    child.kill().ok();
    child.wait().ok();

    let seen = sink.lock().unwrap().clone();
    assert_eq!(
        (seen[0].1.as_str(), seen[1].1.as_str()),
        ("acct-home", "acct-payer"),
        "the running conversation changed account: {seen:?}"
    );
    // And the home new sessions start in - which is where the transcripts go -
    // never moved.
    let launch = std::fs::read_to_string(store.join("active-codex")).unwrap();
    assert!(
        launch.trim().ends_with("aaaa1111"),
        "the transcript store is untouched: {launch}"
    );
}

/// Codex renders `model_providers.<id>.name` on its /status screen. With the
/// proxy in the middle, the auth.json inside CODEX_HOME is NOT the account that
/// pays for the turn - the proxy replaces its bearer on the way out. So the one
/// place Codex shows an identity shows the wrong one, and the account actually
/// being charged appears nowhere on the screen. The provider name is the only
/// field we control that Codex prints, so the paying account goes there.
mod codex_status_names_the_payer {
    use std::path::Path;
    use swapdex::shim::codex_shim_script;

    /// Run the generated shim with stubs standing in for swapdex and codex, and
    /// return the argument line codex was handed.
    fn args_codex_receives(serving: &str, port: &str) -> String {
        let tmp = std::env::temp_dir().join(format!(
            "sx-shim-{}-{}",
            std::process::id(),
            serving.len() * 7 + port.len()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let sx = tmp.join("swapdex");
        // The stub answers both questions the shim asks: the proxy port, and
        // who is serving. An empty answer is how "nobody" arrives.
        std::fs::write(
            &sx,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do\n\tcase \"$a\" in\n\t--ensure) echo '{port}'; exit 0 ;;\n\tserve) shift; printf '%s' '{serving}'; exit 0 ;;\n\tesac\ndone\nexit 0\n"
            ),
        )
        .unwrap();
        let codex = tmp.join("codex");
        std::fs::write(&codex, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        let shim = tmp.join("shim");
        std::fs::write(&shim, codex_shim_script(&tmp.join("ptr"), &codex, &sx)).unwrap();
        for f in [&sx, &codex, &shim] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = std::process::Command::new("sh")
            .arg(&shim)
            .arg("hello")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn the_provider_name_carries_the_account_that_pays() {
        let got = args_codex_receives("work", "8788");
        assert!(
            got.lines()
                .any(|l| l == "model_providers.swapdex.name=swapdex: work"),
            "the /status provider names the payer, got:\n{got}"
        );
    }

    #[test]
    fn with_nobody_serving_the_name_claims_no_account() {
        let got = args_codex_receives("", "8788");
        assert!(
            got.lines()
                .any(|l| l == "model_providers.swapdex.name=swapdex"),
            "a bare name when no account directs turns, got:\n{got}"
        );
        assert!(
            !got.contains("name=swapdex: "),
            "and never a dangling label, got:\n{got}"
        );
    }

    /// A reading command takes no provider override at all, so it must also not
    /// pay the cost of asking who serves.
    #[test]
    fn a_reading_command_asks_nothing() {
        let s = codex_shim_script(
            Path::new("/store/active-codex"),
            Path::new("/usr/bin/codex"),
            Path::new("/usr/bin/swapdex"),
        );
        let guard = s.find("sx_plain=no").unwrap();
        let ask = s.find("serve --tool codex").expect("asks who serves");
        let name = s.find("model_providers.swapdex.name").unwrap();
        assert!(guard < ask && ask < name, "asked inside the talking branch");
    }
}

/// The proxy, handed an account with no login, gets out of the way: it forwards
/// the CLIENT's own credential so the turn still works. That is right for a turn
/// and wrong for everything around it - the dashboard, `serve`, and the Codex
/// status line all go on naming an account that is not paying, while the user's
/// own account quietly is. swapdex exists to make the account you think is
/// paying be the one paying, so this state must not be reachable, and where it
/// is reachable anyway it must not be reported as if it were fine.
mod an_account_that_cannot_pay {
    use swapdex::commands::{self, ToolSel};
    use swapdex::paths::Paths;
    use swapdex::slots::Slots;

    #[test]
    fn cannot_be_handed_the_turns() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut s = Slots::open_for(&paths, "codex").unwrap();
            s.create("work").unwrap();
        }
        let code = commands::serve(&paths, Some("work"), false, Some(ToolSel::Codex), false)
            .expect("serve returns a code rather than failing");
        assert_eq!(code, 6, "refused");
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().serving_dir(),
            None,
            "and nothing was pointed at it"
        );
    }

    /// Reachable anyway: a default pointer can name a slot that was created and
    /// never signed into. The label then has to say so rather than claim it pays.
    #[test]
    fn is_labelled_as_one_when_it_is_the_default() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut s = Slots::open_for(&paths, "codex").unwrap();
            s.create("work").unwrap();
            s.set_default("work").unwrap();
        }
        assert_eq!(
            commands::payer_label(&paths, "codex").as_deref(),
            Some("work (no login)")
        );
    }

    #[test]
    fn and_plainly_when_the_login_is_there() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let dir = {
            let mut s = Slots::open_for(&paths, "codex").unwrap();
            let rec = s.create("work").unwrap();
            s.set_default("work").unwrap();
            rec.config_dir
        };
        std::fs::write(
            dir.join("auth.json"),
            br#"{"tokens":{"access_token":"a","account_id":"acc"}}"#,
        )
        .unwrap();
        assert_eq!(
            commands::payer_label(&paths, "codex").as_deref(),
            Some("work")
        );
    }
}

/// On macOS a Claude login lives in the Keychain, not in a file, and a Keychain
/// that will not open reads exactly like an account that was never signed into.
/// The difference matters: one is "sign in", the other is "you are signed in,
/// this shell just cannot see it". A guard that confuses them refuses a working
/// account and sends the user to fix something that is not broken.
mod a_locked_keychain_is_not_a_missing_login {
    use swapdex::proxy::creds::TokenUnavailable;
    use swapdex::proxy::login_present;

    #[test]
    fn locked_still_counts_as_signed_in() {
        assert!(login_present(Err(TokenUnavailable::KeychainLocked)));
    }

    #[test]
    fn only_a_missing_one_counts_as_absent() {
        assert!(!login_present(Err(TokenUnavailable::NoLogin)));
    }
}

/// The dashboard marks one row per tool as the active one. Claude's rows asked
/// the proxy who was serving and fell back to the pointer; Codex's rows asked
/// only the pointer. So on the Codex side, pressing Enter - which hands turns to
/// that account - moved who pays and left the mark exactly where it was, and the
/// change read as nothing having happened.
mod the_active_mark_follows_who_pays {
    use swapdex::commands::active_slot_name;
    use swapdex::paths::Paths;
    use swapdex::slots::Slots;

    #[test]
    fn on_codex_as_much_as_on_claude() {
        for tool in ["codex", "claude-code"] {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths::rooted(root.path());
            {
                let mut s = Slots::open_for(&paths, tool).unwrap();
                s.create("first").unwrap();
                s.create("second").unwrap();
                s.set_default("first").unwrap();
            }
            assert_eq!(
                active_slot_name(&paths, tool).as_deref(),
                Some("first"),
                "{tool}: with nobody serving, the pointer decides"
            );
            Slots::open_for(&paths, tool)
                .unwrap()
                .set_serving("second")
                .unwrap();
            assert_eq!(
                active_slot_name(&paths, tool).as_deref(),
                Some("second"),
                "{tool}: and handing turns over moves the mark"
            );
        }
    }
}

/// A bare `swapdex` opens the dashboard when there are accounts to show. It
/// decided that by counting saved PROFILES and live logins - and never the
/// slots, which is what `run`, `adopt`, and `onboard` all create. So the model
/// swapdex steers people into did not count as having accounts, and a user whose
/// accounts are all slots got a banner where the picker should have been.
mod accounts_worth_opening_the_dashboard_for {
    use swapdex::commands::has_any_account;
    use swapdex::paths::Paths;
    use swapdex::slots::Slots;

    #[test]
    fn a_slot_is_an_account() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        assert!(!has_any_account(&paths), "nothing yet");
        Slots::open_for(&paths, "codex")
            .unwrap()
            .create("work")
            .unwrap();
        assert!(has_any_account(&paths), "a slot counts");
    }

    #[test]
    fn and_so_is_a_claude_one() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        Slots::open_for(&paths, "claude-code")
            .unwrap()
            .create("work")
            .unwrap();
        assert!(has_any_account(&paths));
    }
}

/// `serve` is the action that changes who PAYS, and it left no record at all -
/// only `use` and `restore` were written to the timeline. So there was no way to
/// answer "who was paying when this ran", which is what Codex usage attribution
/// needs: the numbers in a Codex transcript come from the token that served
/// those turns, not from the account whose home the file sits in.
///
/// Adding the event is only half of it. The timeline reader dropped the `action`
/// field entirely, so a serve event would have been read as a switch and started
/// answering "which account holds this conversation" - a different question with
/// a different answer.
mod who_was_paying_is_its_own_history {
    use swapdex::commands::{self, ToolSel};
    use swapdex::paths::Paths;
    use swapdex::session_link::{attribute, payer_at, read_timeline};
    use swapdex::slots::Slots;
    use swapdex::store::Store;

    /// Far enough ahead that a real clock reading is behind it. `serve` stamps
    /// its event with the wall clock, so a synthetic query time in the past would
    /// filter out the very event under test.
    const LATER: i64 = 4_000_000_000;

    fn store_with_two_codex_accounts(root: &std::path::Path) -> Paths {
        let paths = Paths::rooted(root);
        let mut s = Slots::open_for(&paths, "codex").unwrap();
        for name in ["home", "payer"] {
            let rec = s.create(name).unwrap();
            std::fs::write(
                rec.config_dir.join("auth.json"),
                br#"{"tokens":{"access_token":"a","account_id":"acc"}}"#,
            )
            .unwrap();
        }
        paths
    }

    #[test]
    fn serving_records_the_payer_without_moving_the_session_attribution() {
        let root = tempfile::tempdir().unwrap();
        let paths = store_with_two_codex_accounts(root.path());
        let store = Store::open(&paths).unwrap();
        store
            .append_timeline_at("codex", "home", "use", 100)
            .unwrap();

        commands::serve(&paths, Some("payer"), false, Some(ToolSel::Codex), false).unwrap();

        let events = read_timeline(&paths);
        assert_eq!(
            attribute(&events, "codex", LATER).as_deref(),
            Some("home"),
            "the conversation still lives where `use` put it"
        );
        assert_eq!(
            payer_at(&events, "codex", LATER).as_deref(),
            Some("payer"),
            "and the turns are paid for by the account handed them"
        );
    }

    /// With nobody ever handed the turns, the account whose home the session runs
    /// in is the one paying - which is exactly what no proxy means.
    #[test]
    fn with_nobody_served_the_home_account_pays() {
        let root = tempfile::tempdir().unwrap();
        let paths = store_with_two_codex_accounts(root.path());
        let store = Store::open(&paths).unwrap();
        store
            .append_timeline_at("codex", "home", "use", 100)
            .unwrap();
        let events = read_timeline(&paths);
        assert_eq!(payer_at(&events, "codex", LATER).as_deref(), Some("home"));
    }
}

/// A hermetic store means a hermetic run. `serve` starts the proxy its setting
/// needs, and that proxy is deliberately detached - it has to outlive the shell
/// that asked for it. Under SWAPDEX_ROOT that is wrong twice over: the daemon
/// outlives the temporary store it was pointed at, and it keeps the port, so a
/// test run leaves a listener bound to 127.0.0.1 answering for a directory that
/// no longer exists. One was found still running hours after its store was gone.
mod a_sandboxed_run_starts_no_daemon {
    use swapdex::commands::{self, ToolSel};
    use swapdex::paths::Paths;
    use swapdex::slots::Slots;

    #[test]
    fn serve_under_a_test_root_leaves_nothing_running() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let rec = Slots::open_for(&paths, "codex")
            .unwrap()
            .create("work")
            .unwrap();
        std::fs::write(
            rec.config_dir.join("auth.json"),
            br#"{"tokens":{"access_token":"a","account_id":"acc"}}"#,
        )
        .unwrap();

        let started = std::time::Instant::now();
        commands::serve(&paths, Some("work"), false, Some(ToolSel::Codex), false).unwrap();
        assert!(
            swapdex::proxy::running_proxy_for(&paths, "codex").is_none(),
            "no daemon was left behind"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "and no time was spent waiting for one to announce itself"
        );
    }
}

/// The proxy writes down which account is serving, and every screen reads it -
/// the dashboard's active mark, and the name Codex prints on /status. It wrote
/// that mark as soon as it CHOSE a slot, before finding out whether that slot
/// could pay. When it cannot, the proxy forwards the client's own credential
/// instead, and the mark stays on an account that paid for nothing - not for one
/// turn, but for as long as that account is chosen.
#[test]
fn the_serving_mark_names_who_actually_paid() {
    let root = tempfile::tempdir().unwrap();
    // Registered, pointed at, and never signed into.
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots").join("cccc3333");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&serde_json::json!([{
            "name": "nologin", "id": "cccc3333", "config_dir": slot, "adopted": false
        }]))
        .unwrap(),
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
    post_through(port, "{\"t\":1}");
    let mark = std::fs::read_to_string(store.join("proxy-serving")).unwrap_or_default();
    child.kill().ok();
    child.wait().ok();

    assert_ne!(
        mark.trim(),
        "nologin",
        "the client's own login paid for that turn, so nothing may claim 'nologin' did"
    );
}

/// Codex records its rate limits into the session transcript of whichever HOME
/// it is running in - but under the proxy those numbers came back on the token
/// of the account SERVING the turns. So a conversation living in A while B pays
/// writes B's usage into A's transcript, and the dashboard showed it on A.
///
/// It also read one fixed directory and required a matching legacy profile, so
/// an account that is only a slot - which is what run, adopt and onboard create -
/// got no usage bar at all.
mod codex_usage_belongs_to_whoever_paid {
    use swapdex::codex_limits;
    use swapdex::paths::Paths;
    use swapdex::slots::Slots;

    fn transcript(dir: &std::path::Path, used_pct: f64) {
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout.jsonl"),
            format!(
                r#"{{"payload":{{"rate_limits":{{"primary":{{"used_percent":{used_pct},"window_minutes":300,"resets_at":4000000000}}}}}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_slot_only_account_still_gets_its_numbers() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let rec = Slots::open_for(&paths, "codex")
            .unwrap()
            .create("work")
            .unwrap();
        transcript(&rec.config_dir, 42.0);

        let got = codex_limits::for_slot(&rec.config_dir, 0, u64::MAX)
            .expect("the slot's own sessions dir is read");
        assert_eq!(got.short.unwrap().used_pct, 42.0);
    }

    /// And the fixed home is no longer the only place looked at: two accounts,
    /// two homes, two different readings.
    #[test]
    fn each_home_reports_its_own() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open_for(&paths, "codex").unwrap();
        let a = s.create("a").unwrap();
        let b = s.create("b").unwrap();
        transcript(&a.config_dir, 10.0);
        transcript(&b.config_dir, 90.0);
        assert_eq!(
            codex_limits::for_slot(&a.config_dir, 0, u64::MAX)
                .unwrap()
                .short
                .unwrap()
                .used_pct,
            10.0
        );
        assert_eq!(
            codex_limits::for_slot(&b.config_dir, 0, u64::MAX)
                .unwrap()
                .short
                .unwrap()
                .used_pct,
            90.0
        );
    }
}

/// A Codex upstream that refuses the first turn with 429 and the header that
/// says the refusal is temporary, then serves.
fn fake_codex_throttle_once(sink: Arc<Mutex<Vec<(String, String, String)>>>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        let mut first = true;
        for mut rq in server.incoming_requests() {
            let head = |name: &'static str| {
                rq.headers()
                    .iter()
                    .find(|h| h.field.equiv(name))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default()
            };
            let auth = head("authorization");
            let acct = head("chatgpt-account-id");
            let mut body = Vec::new();
            rq.as_reader().read_to_end(&mut body).ok();
            sink.lock().unwrap().push((auth, acct, String::new()));
            let throttled = first;
            first = false;
            let resp = if throttled {
                tiny_http::Response::from_string("{\"error\":\"slow down\"}")
                    .with_status_code(429)
                    .with_header(
                        tiny_http::Header::from_bytes(&b"x-should-retry"[..], &b"true"[..])
                            .unwrap(),
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(&b"retry-after"[..], &b"1"[..]).unwrap(),
                    )
            } else {
                tiny_http::Response::from_string("{\"ok\":true}").with_status_code(200)
            };
            let _ = rq.respond(resp);
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn post_codex(port: u16) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let _ = agent
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("authorization", "Bearer CLIENT-OWN")
        .header("chatgpt-account-id", "acct-client")
        .send("{\"t\":1}");
}

/// `--account` pins the proxy to one account: every turn is that account's, and
/// a refusal is that account's answer to give. Claude's retry path checks the
/// pin before rotating; Codex's did not, so a pinned run quietly billed a
/// different account the moment the pinned one was refused.
#[test]
fn a_pinned_codex_account_is_never_rotated_away_from() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "cccc1111",
        "AT-WORK",
        "acct-work",
        true,
    );
    seed_codex_slot(
        root.path(),
        "home",
        "dddd2222",
        "AT-HOME",
        "acct-home",
        false,
    );
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_throttle_once(sink.clone());
    let (mut child, port) =
        start_codex_proxy(root.path(), &upstream, &["--auto", "--account", "work"]);

    post_codex(port);
    child.kill().ok();
    child.wait().ok();

    let seen = sink.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|(auth, acct, _)| auth.contains("AT-WORK") && acct == "acct-work"),
        "the pinned account served every attempt, saw: {seen:?}"
    );
}

/// A 429 wears two meanings, and Codex's path only knew one. Every 429 marked
/// the account spent and moved the turn elsewhere - so "slow down for a second",
/// which the response says explicitly, cost the user their account for the life
/// of the proxy.
#[test]
fn a_throttled_codex_turn_stays_on_the_same_account() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "cccc1111",
        "AT-WORK",
        "acct-work",
        true,
    );
    seed_codex_slot(
        root.path(),
        "home",
        "dddd2222",
        "AT-HOME",
        "acct-home",
        false,
    );
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_throttle_once(sink.clone());
    let (mut child, port) = start_codex_proxy(root.path(), &upstream, &["--auto"]);

    post_codex(port);
    child.kill().ok();
    child.wait().ok();

    let seen = sink.lock().unwrap().clone();
    assert!(seen.len() >= 2, "the throttled turn was retried: {seen:?}");
    assert!(
        seen.iter().all(|(auth, _, _)| auth.contains("AT-WORK")),
        "and on the same account, not by giving it away: {seen:?}"
    );
}

/// `observed_at` is what the dashboard uses to say how old a Codex reading is -
/// there is no endpoint to ask, so the age IS the caveat. It was taken from the
/// transcript's mtime, which moves every time Codex writes anything at all. A
/// conversation that keeps running without the API restating the windows made an
/// hours-old snapshot look like it had just been taken.
#[test]
fn a_codex_reading_is_as_old_as_the_record_not_the_file() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // One record carrying limits, stamped long ago; then later chatter with no
    // limits at all, which is what keeps moving the file's mtime.
    std::fs::write(
        sessions.join("rollout.jsonl"),
        concat!(
            r#"{"timestamp":"2026-03-10T11:53:41.974Z","type":"event_msg","payload":{"info":{"rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":4000000000}}}}}"#,
            "\n",
            r#"{"timestamp":"2026-03-10T23:00:00.000Z","type":"event_msg","payload":{"type":"agent_message"}}"#,
            "\n"
        ),
    )
    .unwrap();

    let got = swapdex::codex_limits::for_slot(root.path(), 0, u64::MAX).expect("limits found");
    let stamped = swapdex::session_link::rfc3339_to_secs("2026-03-10T11:53:41.974Z").unwrap();
    assert_eq!(
        got.observed_at,
        Some(stamped),
        "the reading is as old as the moment the API stated it"
    );
}

/// Enter hands turns to an account, and the mark has to follow - that is the
/// only thing on screen saying it worked. Slot rows were taught the full order
/// of authority (a running proxy's own record, else the serving pointer, else
/// the default); PROFILE rows were not, and still asked only "is this the
/// DEFAULT account?". An account that is both - a saved profile and a slot -
/// draws as one row, and when the profile half won that merge, pressing Enter
/// moved who pays and left the row reading "ready".
mod the_mark_follows_serve_on_every_kind_of_row {
    use swapdex::commands::active_slot_name;
    use swapdex::paths::Paths;
    use swapdex::slots::Slots;

    #[test]
    fn one_resolver_answers_for_slots_and_profiles_alike() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut s = Slots::open_for(&paths, "claude-code").unwrap();
            s.create("bsgong").unwrap();
            s.create("rnd").unwrap();
            s.set_default("bsgong").unwrap();
        }
        assert_eq!(
            active_slot_name(&paths, "claude-code").as_deref(),
            Some("bsgong")
        );
        Slots::open_for(&paths, "claude-code")
            .unwrap()
            .set_serving("rnd")
            .unwrap();
        assert_eq!(
            active_slot_name(&paths, "claude-code").as_deref(),
            Some("rnd"),
            "serve moved the payer, so the mark moves - with no proxy running too"
        );
    }
}

/// The order of authority had the past outranking the instruction. `serve` is
/// what the user just asked for; `proxy-serving` is what the proxy last actually
/// did - and until the next turn goes out, that is the OLD account. So pressing
/// Enter changed who pays and the row went on naming the previous one, with
/// nothing to say the key had worked.
///
/// A rotation still shows: it happens when nobody asked for anything, which is
/// exactly when the proxy's own record is the only answer there is.
mod an_instruction_outranks_what_already_happened {
    use swapdex::commands::active_slot_name;
    use swapdex::paths::Paths;
    use swapdex::slots::Slots;

    #[test]
    fn serve_shows_at_once_even_before_a_turn_goes_out() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut s = Slots::open_for(&paths, "claude-code").unwrap();
            s.create("bsgong").unwrap();
            s.create("rnd").unwrap();
            s.set_default("bsgong").unwrap();
            s.set_serving("rnd").unwrap();
        }
        // What the proxy last did, which is still the previous account.
        std::fs::write(paths.store_dir().join("proxy-serving"), b"bsgong").unwrap();

        assert_eq!(
            active_slot_name(&paths, "claude-code").as_deref(),
            Some("rnd"),
            "the account just handed the turns is the one marked"
        );
    }

    /// The order itself, without needing a live proxy to observe it.
    #[test]
    fn asked_for_beats_what_happened_beats_the_default() {
        let n = |s: &str| Some(s.to_string());
        use swapdex::commands::pick_active;
        assert_eq!(
            pick_active(n("rnd"), n("bsgong"), n("bsgong")),
            n("rnd"),
            "the instruction wins even though the proxy has not caught up"
        );
        assert_eq!(
            pick_active(None, n("spare"), n("bsgong")),
            n("spare"),
            "nobody asked, so a rotation is the only thing that knows"
        );
        assert_eq!(
            pick_active(None, None, n("bsgong")),
            n("bsgong"),
            "and otherwise, where sessions start"
        );
        assert_eq!(pick_active(None, None, None), None);
    }
}

/// A refusal we cannot rotate around still goes back as a 429 - that is true.
/// But Claude Code reads a `Retry-After` over 20s as "cool down for thirty
/// minutes", so relaying a spent window's hour-long wait sidelines the user for
/// half an hour over something they could step around by pressing Enter. When
/// another account could take the turn, the wait handed back is capped.
fn upstream_refusing_with_a_long_wait(sink: Arc<Mutex<Vec<Seen>>>) -> String {
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
            sink.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            let resp = tiny_http::Response::from_string("{\"error\":\"spent\"}")
                .with_status_code(429)
                .with_header(
                    tiny_http::Header::from_bytes(&b"retry-after"[..], &b"3600"[..]).unwrap(),
                )
                .with_header(
                    tiny_http::Header::from_bytes(
                        &b"anthropic-ratelimit-unified-status"[..],
                        &b"rejected"[..],
                    )
                    .unwrap(),
                );
            let _ = rq.respond(resp);
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_refusal_does_not_cool_the_client_down_for_half_an_hour() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "spare", "bbbb2222", "AT-SPARE", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = upstream_refusing_with_a_long_wait(sink.clone());
    // --no-auto: the proxy will not rotate, so the refusal reaches the client.
    let (mut child, port) = start_proxy(root.path(), &upstream, &["--no-auto"]);

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let resp = agent
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("authorization", "Bearer CLIENT-OWN")
        .send("{\"t\":1}")
        .unwrap();
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap_or("").to_string());
    child.kill().ok();
    child.wait().ok();

    assert_eq!(status, 429, "the refusal is still a refusal");
    assert_eq!(
        retry_after.as_deref(),
        Some("20"),
        "capped, because `spare` could have taken this turn"
    );
}

/// An upstream that refuses the FIRST account it sees with 403 (a lapsed
/// subscription) and serves anybody else.
fn upstream_refusing_one_account(sink: Arc<Mutex<Vec<Seen>>>, unentitled: &'static str) -> String {
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
            let barred = auth.contains(unentitled);
            sink.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            let resp = if barred {
                tiny_http::Response::from_string("{\"error\":\"not entitled\"}")
                    .with_status_code(403)
            } else {
                tiny_http::Response::from_string("{\"ok\":true}").with_status_code(200)
            };
            let _ = rq.respond(resp);
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// One account whose subscription lapsed used to answer for the whole fleet:
/// every turn landed on it, got a 403, and stopped - while accounts with quota
/// sat unused. 403 says "this ACCOUNT cannot serve", the same shape as 401 and
/// 429, so the turn moves along.
#[test]
fn a_lapsed_subscription_does_not_block_the_fleet() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "lapsed", "aaaa1111", "AT-LAPSED", true);
    seed_slot(root.path(), "good", "bbbb2222", "AT-GOOD", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = upstream_refusing_one_account(sink.clone(), "AT-LAPSED");
    let (mut child, port) = start_proxy(root.path(), &upstream, &["--auto"]);

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let resp = agent
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("authorization", "Bearer CLIENT-OWN")
        .send("{\"t\":1}")
        .unwrap();
    let status = resp.status();
    child.kill().ok();
    child.wait().ok();

    let seen = auths(&sink);
    assert_eq!(status, 200, "the turn was served, not abandoned: {seen:?}");
    assert!(
        seen.iter().any(|a| a.contains("AT-GOOD")),
        "it moved to the account that could serve: {seen:?}"
    );
}
