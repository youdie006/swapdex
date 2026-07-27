use std::io::Read;
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

fn post_through(port: u16, body: &str) -> String {
    let mut resp = ureq::post(format!("http://127.0.0.1:{port}/v1/messages"))
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
