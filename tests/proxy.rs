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
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"five_hour\":{\"utilization\":0.99}}\\n200'\n",
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
