use std::io::Read;
use std::process::Command;
use std::sync::{Arc, Mutex};

use swapdex::proxy::ratelimit::{classify_429, from_headers, Throttle};

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[test]
fn rejected_overage_does_not_exhaust_an_allowed_plan() {
    let quota = from_headers(&headers(&[
        ("Anthropic-RateLimit-Unified-Status", " allowed "),
        ("ANTHROPIC-RATELIMIT-UNIFIED-5H-STATUS", "ALLOWED"),
        ("anthropic-ratelimit-unified-overage-status", " ReJeCtEd "),
    ]))
    .expect("unified quota headers were present");

    assert!(
        !quota.rejected,
        "declining optional paid overage must leave included plan quota usable"
    );
    assert!(
        quota.rejected_windows().is_empty(),
        "diagnostics must not label disabled overage as a spent plan window"
    );
}

#[test]
fn rejected_plan_window_still_exhausts_when_overage_is_also_rejected() {
    let quota = from_headers(&headers(&[
        ("anthropic-ratelimit-unified-status", " allowed "),
        ("anthropic-ratelimit-unified-5h-status", " rejected "),
        ("anthropic-ratelimit-unified-overage-status", "rejected"),
    ]))
    .expect("unified quota headers were present");

    assert!(quota.rejected, "an included plan window is exhausted");
    assert_eq!(
        quota.rejected_windows(),
        vec!["5h-status"],
        "logging must name only windows that can exhaust included usage"
    );
}

#[test]
fn overage_status_does_not_discard_independent_window_resets() {
    let quota = from_headers(&headers(&[
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-reset", " 1800000000 "),
        ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
        ("anthropic-ratelimit-unified-7d-reset", "1800600000"),
        ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ("anthropic-ratelimit-unified-overage-reset", "1801200000"),
    ]))
    .expect("unified quota headers were present");

    assert!(!quota.rejected);
    assert_eq!(quota.reset_of("5h"), Some(1_800_000_000));
    assert_eq!(quota.reset_of("7d"), Some(1_800_600_000));
    assert_eq!(quota.reset_of("overage"), Some(1_801_200_000));
    assert_eq!(quota.reset_secs, Some(1_800_000_000));
}

#[test]
fn retryable_overage_only_429_remains_a_bounded_throttle() {
    let result = classify_429(
        &headers(&[
            ("X-Should-Retry", " true "),
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ]),
        0,
    );

    assert_eq!(
        result,
        Throttle::RetryAfter(std::time::Duration::from_secs(1))
    );
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

struct ReapedChild(Option<std::process::Child>);

impl ReapedChild {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    fn stop(mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().expect("proxy child reaped");
        }
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

struct ControlledUpstream {
    url: String,
    server: Arc<tiny_http::Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ControlledUpstream {
    fn overage_allowed_and_plan_exhausted(sink: Arc<Mutex<Vec<String>>>) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let serving = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            for mut request in serving.incoming_requests() {
                let auth = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("authorization"))
                    .map(|header| header.value.as_str().to_string())
                    .unwrap_or_default();
                let mut body = Vec::new();
                request.as_reader().read_to_end(&mut body).ok();
                sink.lock().unwrap().push(auth.clone());

                let mut response = tiny_http::Response::from_string("{\"ok\":true}");
                if auth == "Bearer AT-B" {
                    response = response
                        .with_status_code(429)
                        .with_header(header("anthropic-ratelimit-unified-5h-status", "rejected"))
                        .with_header(header("anthropic-ratelimit-unified-5h-reset", "4102444800"))
                        .with_header(header(
                            "anthropic-ratelimit-unified-overage-status",
                            "rejected",
                        ));
                } else {
                    response = response
                        .with_header(header("anthropic-ratelimit-unified-status", "allowed"))
                        .with_header(header("anthropic-ratelimit-unified-5h-status", "allowed"))
                        .with_header(header("anthropic-ratelimit-unified-5h-reset", "4102444800"))
                        .with_header(header(
                            "anthropic-ratelimit-unified-overage-status",
                            "rejected",
                        ));
                }
                request.respond(response).ok();
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            server,
            thread: Some(thread),
        }
    }

    fn close(mut self) {
        self.server.unblock();
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Drop for ControlledUpstream {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

fn seed_slot(root: &std::path::Path, name: &str, id: &str, token: &str, make_default: bool) {
    let store = root.join(".local/share/swapdex");
    let slot = store.join("slots").join(id);
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        format!(
            r#"{{"oauthAccount":{{"accountUuid":"uuid-of-{name}","emailAddress":"{name}@example.com"}}}}"#
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

    let mut records: Vec<serde_json::Value> = std::fs::read(store.join("slots.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    records.push(serde_json::json!({
        "name": name,
        "id": id,
        "config_dir": slot,
        "adopted": false
    }));
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&records).unwrap(),
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

fn start_proxy(root: &std::path::Path, upstream: &str) -> (ReapedChild, u16) {
    let mut child = Command::new(bin())
        .args(["proxy", "--port", "0", "--auto"])
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_UPSTREAM", upstream)
        .env("SWAPDEX_CURL", "/bin/false")
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.as_mut().unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stdout.read(&mut byte).unwrap_or(0) == 1 {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&line);
    let port = line
        .split_whitespace()
        .filter_map(|word| word.rsplit(':').next())
        .find_map(|part| part.trim().parse::<u16>().ok())
        .unwrap_or_else(|| {
            child.kill().ok();
            child.wait().ok();
            panic!("proxy did not announce a port: {line}")
        });
    (ReapedChild::new(child), port)
}

fn serve_as(root: &std::path::Path, name: &str) {
    let output = Command::new(bin())
        .args(["serve", name])
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap();
    assert!(output.status.success(), "serve {name} failed: {output:?}");
}

fn post_through(port: u16) -> u16 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    agent
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("authorization", "Bearer CLIENT-TOKEN")
        .header("content-type", "application/json")
        .send(b"{}".as_slice())
        .expect("proxy answered")
        .status()
        .as_u16()
}

#[test]
fn available_no_overage_account_stays_selected_and_accepts_a_real_handoff() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "a", "aaaa1111", "AT-A", true);
    seed_slot(root.path(), "b", "bbbb2222", "AT-B", false);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = ControlledUpstream::overage_allowed_and_plan_exhausted(Arc::clone(&seen));
    let (proxy, port) = start_proxy(root.path(), &upstream.url);

    assert_eq!(post_through(port), 200);
    assert_eq!(post_through(port), 200);
    serve_as(root.path(), "b");
    assert_eq!(
        post_through(port),
        200,
        "B's real plan exhaustion should hand the same request to available A"
    );

    proxy.stop();
    upstream.close();
    assert_eq!(
        *seen.lock().unwrap(),
        ["Bearer AT-A", "Bearer AT-A", "Bearer AT-B", "Bearer AT-A"],
        "successful overage-disabled responses keep A selected, then B hands off to it"
    );
}
