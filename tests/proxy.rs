use std::io::Read;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

/// A fake upstream API: records the Authorization header it was given and
/// answers with a small body. No test ever reaches the real API.
fn fake_upstream(auth_sink: Arc<Mutex<Vec<String>>>) -> String {
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
            auth_sink.lock().unwrap().push(auth);
            let _ = rq.respond(tiny_http::Response::from_string("{\"ok\":true}"));
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Write a slot with a known token and make it the default account.
fn seed_slot(root: &std::path::Path, name: &str, id: &str, token: &str, make_default: bool) {
    let store = root.join(".local/share/swapdex");
    let slot = store.join("slots").join(id);
    std::fs::create_dir_all(&slot).unwrap();
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
        sink.lock().unwrap().clone(),
        vec!["Bearer AT-SLOT".to_string()],
        "the slot's token replaced the client's"
    );
}
