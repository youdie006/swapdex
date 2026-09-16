use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A fake upstream that can be stopped and joined before a test returns.
///
/// Most proxy tests predate graceful fake-server cleanup and leave their
/// listener thread for the test process to reap. Timing tests and state-machine
/// regressions need a stronger boundary: nothing from one test may keep running
/// while the next one measures a deadline or observes a pointer.
struct ControlledUpstream {
    url: String,
    server: Arc<tiny_http::Server>,
    thread: Option<std::thread::JoinHandle<()>>,
    workers: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

/// A spawned proxy that is killed and waited even if an assertion unwinds.
struct ReapedChild(Option<std::process::Child>);

impl ReapedChild {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    fn stop(mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().unwrap();
        }
    }

    fn stop_with_stdout(mut self) -> String {
        let Some(mut child) = self.0.take() else {
            return String::new();
        };
        let mut stdout = child.stdout.take().unwrap();
        child.kill().ok();
        child.wait().unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).unwrap();
        output
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

#[cfg(target_os = "linux")]
fn spawn_native_cli(
    root: &std::path::Path,
    comm: &str,
    env: &[(&str, &std::path::Path)],
) -> ReapedChild {
    let binary = root.join(comm);
    std::os::unix::fs::symlink("/bin/sleep", &binary).unwrap();
    let mut command = Command::new(&binary);
    command.arg("120").env_clear();
    for (key, value) in env {
        command.env(key, value);
    }
    let child = command.spawn().unwrap();
    let pid = child.id();
    let child = ReapedChild::new(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim()
        != comm
    {
        assert!(
            std::time::Instant::now() < deadline,
            "fake native {comm} process did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child
}

impl ControlledUpstream {
    fn start(mut respond: impl FnMut(tiny_http::Request) + Send + 'static) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let serving = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            for request in serving.incoming_requests() {
                respond(request);
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            server,
            thread: Some(thread),
            workers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A controllable upstream whose requests may complete out of order.
    fn start_concurrent(respond: impl Fn(tiny_http::Request) + Send + Sync + 'static) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let serving = Arc::clone(&server);
        let respond = Arc::new(respond);
        let workers = Arc::new(Mutex::new(Vec::new()));
        let spawned = Arc::clone(&workers);
        let thread = std::thread::spawn(move || {
            for request in serving.incoming_requests() {
                let respond = Arc::clone(&respond);
                spawned
                    .lock()
                    .unwrap()
                    .push(std::thread::spawn(move || respond(request)));
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            server,
            thread: Some(thread),
            workers,
        }
    }

    fn join_workers(&self) {
        for worker in self.workers.lock().unwrap().drain(..) {
            worker.join().unwrap();
        }
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn close(mut self) {
        self.server.unblock();
        self.thread.take().unwrap().join().unwrap();
        self.join_workers();
    }
}

impl Drop for ControlledUpstream {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.server.unblock();
            thread.join().unwrap();
        }
        self.join_workers();
    }
}

/// A raw loopback upstream that fully reads each request, resets the first TCP
/// connection, then answers later requests. This distinguishes an ambiguous
/// post-send failure from a connect failure without involving a real API.
type RawRequest = (String, Vec<u8>);

struct ResetAfterReadUpstream {
    url: String,
    seen: Arc<Mutex<Vec<RawRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ResetAfterReadUpstream {
    fn start() -> Self {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                // macOS inherits the listener's nonblocking mode on accept.
                // Wait for the body before simulating a post-send reset.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let Some((method, body)) = read_http_request(&mut stream) else {
                    continue;
                };
                let first = {
                    let mut seen = sink.lock().unwrap();
                    seen.push((method, body));
                    seen.len() == 1
                };
                if first {
                    use std::os::fd::AsRawFd;
                    let linger = libc::linger {
                        l_onoff: 1,
                        l_linger: 0,
                    };
                    unsafe {
                        libc::setsockopt(
                            stream.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_LINGER,
                            (&linger as *const libc::linger).cast(),
                            std::mem::size_of::<libc::linger>() as libc::socklen_t,
                        );
                    }
                    drop(stream);
                    continue;
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                    )
                    .ok();
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn seen(&self) -> Vec<RawRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Drop for ResetAfterReadUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(at) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]);
    let method = head.split_whitespace().next()?.to_string();
    let content_length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() - head_end < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Some((method, bytes[head_end..head_end + content_length].to_vec()))
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

/// A Claude upstream that records the untouched request and refuses it. The
/// refusal is intentional: passthrough must return that one answer directly,
/// never retry with a managed credential.
fn refusing_claude_upstream(sink: Arc<Mutex<Vec<Seen>>>) -> String {
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
                .and_then(|v| v["metadata"]["user_id"].as_str().map(str::to_string));
            sink.lock().unwrap().push(Seen { auth, user_id });
            let _ = rq.respond(
                tiny_http::Response::from_string("{\"error\":\"client refused\"}")
                    .with_status_code(401),
            );
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

/// A controllable wall: the next request made with account A is refused as
/// spent, while B and later A requests succeed. This makes an automatic A -> B
/// rotation deterministic without involving a real service.
fn rotating_upstream(
    sink: Arc<Mutex<Vec<Seen>>>,
    reject_next_a: Arc<AtomicBool>,
) -> ControlledUpstream {
    ControlledUpstream::start(move |mut request| {
        let auth = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("authorization"))
            .map(|h| h.value.as_str().to_string())
            .unwrap_or_default();
        let mut body = Vec::new();
        request.as_reader().read_to_end(&mut body).ok();
        let user_id = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["metadata"]["user_id"].as_str().map(str::to_string));
        sink.lock().unwrap().push(Seen {
            auth: auth.clone(),
            user_id,
        });
        let spent = auth == "Bearer AT-A" && reject_next_a.swap(false, Ordering::SeqCst);
        let mut response = tiny_http::Response::from_string("{\"ok\":true}");
        if spent {
            let reset = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string();
            response = response
                .with_status_code(429)
                .with_header(
                    tiny_http::Header::from_bytes(
                        &b"anthropic-ratelimit-unified-status"[..],
                        &b"rejected"[..],
                    )
                    .unwrap(),
                )
                // This fake refuses A once, then accepts its next turn. Give the
                // proxy the matching reset boundary so this fixture exercises a
                // stale rotation rather than a still-live quota bench.
                .with_header(
                    tiny_http::Header::from_bytes(
                        &b"anthropic-ratelimit-unified-reset"[..],
                        reset.as_bytes(),
                    )
                    .unwrap(),
                );
        }
        let _ = request.respond(response);
    })
}

fn serve_as(root: &std::path::Path, name: &str) {
    let output = Command::new(bin())
        .args(["serve", name])
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap();
    assert!(output.status.success(), "serve {name} failed: {output:?}");
}

fn serve_off(root: &std::path::Path) {
    let output = Command::new(bin())
        .args(["serve", "--off"])
        .env("SWAPDEX_ROOT", root)
        .output()
        .unwrap();
    assert!(output.status.success(), "serve --off failed: {output:?}");
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
    start_proxy_with_env(root, upstream, extra, &[])
}

fn start_proxy_with_env(
    root: &std::path::Path,
    upstream: &str,
    extra: &[&str],
    env: &[(&str, &str)],
) -> (std::process::Child, u16) {
    let mut args = vec!["proxy", "--port", "0"];
    args.extend_from_slice(extra);
    let mut child = Command::new(bin())
        .args(&args)
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_UPSTREAM", upstream)
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .env("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        // Individual fixtures replace this with their fake usage endpoint.
        // Streaming/routing tests must not wait on external provider usage.
        .env("SWAPDEX_CURL", "/bin/false")
        .envs(env.iter().copied())
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
    let port = parse_port(&line).unwrap_or_else(|| {
        child.kill().ok();
        child.wait().ok();
        panic!("proxy did not announce a port: {line}")
    });
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

fn post_through_status(port: u16, body: &str) -> u16 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    agent
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("authorization", "Bearer CLIENT-TOKEN")
        .header("content-type", "application/json")
        .send(body.as_bytes())
        .expect("proxy answered")
        .status()
        .as_u16()
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

/// `serve --off` is an explicit, durable passthrough mode. It outranks a proxy
/// pin, survives a proxy restart and pruning, and a failed upstream request is
/// forwarded once with the client's auth and body identity untouched.
#[test]
fn claude_serve_off_is_durable_passthrough_and_beats_a_pin() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    seed_slot(root.path(), "bsgong", "bbbb2222", "AT-BSGONG", false);
    let off = Command::new(bin())
        .args(["serve", "--off"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert!(off.status.success(), "serve --off failed: {off:?}");

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = refusing_claude_upstream(sink.clone());
    let body = r#"{"metadata":{"user_id":"{\"account_uuid\":\"uuid-of-rnd\"}"}}"#;
    let (mut first, port) = start_proxy(root.path(), &upstream, &["--auto", "--account", "bsgong"]);
    assert_eq!(post_through_status(port, body), 401);
    first.kill().ok();
    first.wait().ok();

    // Pruning invalid account pointers must preserve the explicit off marker.
    swapdex::slots::Slots::open_for(&swapdex::paths::Paths::rooted(root.path()), "claude-code")
        .unwrap()
        .prune_serving();
    let marker = root.path().join(".local/share/swapdex/serving-claude");
    assert!(marker.exists(), "off was represented as pointer absence");

    let (mut restarted, port) =
        start_proxy(root.path(), &upstream, &["--auto", "--account", "bsgong"]);
    assert_eq!(post_through_status(port, body), 401);
    restarted.kill().ok();
    restarted.wait().ok();

    let seen = sink.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "a failed passthrough was retried: {seen:?}");
    assert!(
        seen.iter().all(|r| r.auth == "Bearer CLIENT-TOKEN"),
        "managed auth overrode passthrough: {seen:?}"
    );
    assert!(
        seen.iter().all(|r| r
            .user_id
            .as_deref()
            .is_some_and(|id| id.contains("uuid-of-rnd") && !id.contains("uuid-of-bsgong"))),
        "the client's body identity was substituted: {seen:?}"
    );
}

/// Serving state is independent of the account registry. Once passthrough was
/// explicitly selected, a corrupt registry and a pin naming no account must not
/// make the proxy look for a managed credential before it honours that choice.
#[test]
fn serve_off_works_with_no_readable_slots_and_a_missing_pin() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join(".local/share/swapdex");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("serving-claude"), b"off").unwrap();
    std::fs::write(store.join("slots.json"), b"not json").unwrap();

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = ControlledUpstream::start({
        let sink = Arc::clone(&sink);
        move |mut request| {
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let user_id = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["metadata"]["user_id"].as_str().map(str::to_string));
            sink.lock().unwrap().push(Seen { auth, user_id });
            let _ = request.respond(tiny_http::Response::from_string("{\"ok\":true}"));
        }
    });

    let (proxy, port) = start_proxy(root.path(), upstream.url(), &["--account", "missing"]);
    let proxy = ReapedChild::new(proxy);
    let body = r#"{"metadata":{"user_id":"client-body"}}"#;
    assert_eq!(post_through_status(port, body), 200);
    proxy.stop();
    upstream.close();

    let seen = sink.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "passthrough was not a single attempt: {seen:?}"
    );
    assert_eq!(seen[0].auth, "Bearer CLIENT-TOKEN");
    assert_eq!(seen[0].user_id.as_deref(), Some("client-body"));
}

/// An unreadable or malformed state marker is not legacy pointer absence. The
/// proxy must fail the request locally instead of silently spending a managed
/// account, and must not retry that state-read failure upstream.
#[test]
fn unreadable_or_invalid_serving_state_never_uses_managed_auth() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "a", "aaaa1111", "AT-A", true);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = ControlledUpstream::start({
        let sink = Arc::clone(&sink);
        move |mut request| {
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).ok();
            sink.lock().unwrap().push(Seen {
                auth,
                user_id: None,
            });
            let _ = request.respond(tiny_http::Response::from_string("unexpected"));
        }
    });
    let (proxy, port) = start_proxy(root.path(), upstream.url(), &[]);
    let proxy = ReapedChild::new(proxy);
    let marker = root.path().join(".local/share/swapdex/serving-claude");

    std::fs::write(&marker, [0xff]).unwrap();
    assert_eq!(post_through_status(port, "{}"), 502);
    std::fs::write(&marker, b"relative-marker").unwrap();
    assert_eq!(post_through_status(port, "{}"), 502);

    proxy.stop();
    upstream.close();
    assert!(
        sink.lock().unwrap().is_empty(),
        "a serving-state error reached managed upstream auth"
    );
}

/// The proxy may rotate from A to B, but spelling `serve A` again is a new
/// human decision even though the pointer text is byte-for-byte unchanged.
#[test]
fn repeating_the_same_explicit_account_resets_a_stale_rotation() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "a", "aaaa1111", "AT-A", true);
    seed_slot(root.path(), "b", "bbbb2222", "AT-B", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let reject_next_a = Arc::new(AtomicBool::new(true));
    let upstream = rotating_upstream(Arc::clone(&sink), reject_next_a);
    let (proxy, port) = start_proxy(root.path(), upstream.url(), &["--auto"]);
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_through_status(port, "{}"), 200);
    serve_as(root.path(), "a");
    assert_eq!(post_through_status(port, "{}"), 200);

    proxy.stop();
    upstream.close();
    assert_eq!(
        auths(&sink),
        ["Bearer AT-A", "Bearer AT-B", "Bearer AT-A"],
        "the repeated explicit choice lost to the old automatic rotation"
    );
}

/// The entire off -> on transition can happen between two requests. Observing
/// only pointer text would miss both writes and leave B in control.
#[test]
fn off_then_same_account_on_between_requests_resets_a_stale_rotation() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "a", "aaaa1111", "AT-A", true);
    seed_slot(root.path(), "b", "bbbb2222", "AT-B", false);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let reject_next_a = Arc::new(AtomicBool::new(true));
    let upstream = rotating_upstream(Arc::clone(&sink), reject_next_a);
    let (proxy, port) = start_proxy(root.path(), upstream.url(), &["--auto"]);
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_through_status(port, "{}"), 200);
    serve_off(root.path());
    serve_as(root.path(), "a");
    assert_eq!(post_through_status(port, "{}"), 200);

    proxy.stop();
    upstream.close();
    assert_eq!(
        auths(&sink),
        ["Bearer AT-A", "Bearer AT-B", "Bearer AT-A"],
        "off -> on between requests left the old rotation in control"
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
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
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

fn assert_subpercent_threshold_routes(pinned: bool, consume_first: bool) {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "nearly", "aaaa1111", "AT-NEARLY", true);
    seed_slot(root.path(), "fresh", "bbbb2222", "AT-FRESH", false);
    let paths = swapdex::paths::Paths::rooted(root.path());
    swapdex::settings::save(
        &paths,
        &swapdex::settings::Settings {
            proxy_threshold: (!pinned).then_some(0.005),
            proxy_strategy: Some(
                if consume_first {
                    "consume-first"
                } else {
                    "roomiest"
                }
                .into(),
            ),
            ..Default::default()
        },
    )
    .unwrap();
    let curl = fake_curl(root.path(), "AT-NEARLY");
    let script = std::fs::read_to_string(&curl)
        .unwrap()
        .replace("99.0", "1.0")
        .replace("4.0", "0.1");
    std::fs::write(&curl, script).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.to_string());
        sink.lock().unwrap().push(auth);
        request
            .respond(tiny_http::Response::from_string("{\"ok\":true}"))
            .unwrap();
    });
    let extra: &[&str] = if pinned {
        &["--auto", "--threshold", "0.005"]
    } else {
        &["--auto"]
    };
    let curl_value = curl.to_string_lossy().into_owned();
    let (child, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        extra,
        &[("SWAPDEX_CURL", &curl_value)],
    );
    let proxy = ReapedChild::new(child);
    post_through(port, "{\"turn\":1}");
    let output = proxy.stop_with_stdout();
    upstream.close();
    let expected = if consume_first {
        "Bearer AT-FRESH"
    } else {
        "Bearer AT-NEARLY"
    };
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Some(expected.to_string())],
        "0.5% threshold, pinned={pinned}, consume_first={consume_first}: {output}"
    );
    assert!(
        output.contains("at 0.5% used"),
        "threshold display differs: {output}"
    );
    if !consume_first {
        assert!(
            !output.contains("every account is refusing turns"),
            "a 10-point movement margin is not a provider refusal: {output}"
        );
        assert!(
            output.contains("no eligible alternative account"),
            "{output}"
        );
    }
}

#[test]
fn a_saved_subpercent_threshold_rotates_at_the_configured_value() {
    assert_subpercent_threshold_routes(false, true);
}

#[test]
fn a_pinned_subpercent_threshold_rotates_at_the_configured_value() {
    assert_subpercent_threshold_routes(true, true);
}

#[test]
fn a_threshold_movement_margin_does_not_claim_healthy_accounts_are_refusing() {
    assert_subpercent_threshold_routes(false, false);
}

/// A missing reading must not disappear from the evidence for "every account".
#[test]
fn a_threshold_corner_keeps_unmeasured_alternatives_in_its_diagnosis() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "nearly", "aaaa1111", "AT-NEARLY", true);
    seed_slot(root.path(), "full", "bbbb2222", "AT-FULL", false);
    seed_slot(root.path(), "unknown", "cccc3333", "AT-UNKNOWN", false);
    let curl = fake_curl(root.path(), "AT-UNKNOWN");
    let script = std::fs::read_to_string(&curl)
        .unwrap()
        .replace("99.0", "null")
        .replace("4.0", "99.0");
    std::fs::write(&curl, script).unwrap();
    let upstream = ControlledUpstream::start(|request| {
        request
            .respond(tiny_http::Response::from_string("{}"))
            .unwrap();
    });
    let (child, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &["--auto", "--threshold", "0.98"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    let proxy = ReapedChild::new(child);
    post_through(port, "{}");
    let output = proxy.stop_with_stdout();
    upstream.close();
    assert!(
        !output.contains("every account is past the threshold"),
        "{output}"
    );
    assert!(
        output.contains("no eligible alternative account"),
        "{output}"
    );
}

/// One refusal cannot establish what a disabled alternative would have done.
#[test]
fn a_refusal_corner_does_not_count_a_disabled_account_as_refusing() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "one", "aaaa1111", "AT-ONE", true);
    seed_slot(root.path(), "disabled", "bbbb2222", "AT-DISABLED", false);
    let paths = swapdex::paths::Paths::rooted(root.path());
    swapdex::settings::save(
        &paths,
        &swapdex::settings::Settings {
            disabled: vec!["disabled".into()],
            fallback_model: Some("claude-sonnet-5".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let curl = fake_curl(root.path(), "UNUSED");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.to_string())
            .unwrap();
        sink.lock().unwrap().push(auth);
        request
            .respond(tiny_http::Response::from_string("{}").with_status_code(403))
            .unwrap();
    });
    let (child, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &["--auto"],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    let proxy = ReapedChild::new(child);
    for _ in 0..2 {
        post_through(port, r#"{"model":"claude-opus-5","messages":[]}"#);
    }
    let output = proxy.stop_with_stdout();
    upstream.close();
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["Bearer AT-ONE", "Bearer AT-ONE"]
    );
    assert!(
        !output.contains("every account is refusing turns"),
        "{output}"
    );
    assert!(
        output.contains("no eligible alternative account"),
        "{output}"
    );
}

#[test]
fn invalid_proxy_thresholds_fail_before_a_listener_is_started() {
    for value in ["0", "-0.1", "1.01", "NaN", "inf", "-inf"] {
        let root = tempfile::tempdir().unwrap();
        let child = Command::new(bin())
            .args(["proxy", "--port", "0", &format!("--threshold={value}")])
            .env("SWAPDEX_ROOT", root.path())
            .env("SWAPDEX_UPSTREAM", "http://127.0.0.1:1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut child = ReapedChild::new(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(status) = child.0.as_mut().unwrap().try_wait().unwrap() {
                assert_eq!(status.code(), Some(2), "invalid threshold {value}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "proxy accepted invalid threshold {value} and kept running"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!root.path().join(".local/share/swapdex/proxy").exists());
    }
}

/// A quota read happens outside the choice lock and may take long enough for a
/// human to turn managed serving off. The old request must stop rather than
/// repeatedly reselecting the now-inapplicable account or forwarding its token.
#[test]
fn serving_off_during_a_blocked_preemptive_choice_exits_without_forwarding() {
    struct ReleaseOnDrop(std::path::PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            std::fs::write(&self.0, b"release").ok();
        }
    }

    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "a", "aaaa1111", "AT-A", true);
    let started = root.path().join("measurement-started");
    let release = root.path().join("measurement-release");
    let _release_on_drop = ReleaseOnDrop(release.clone());
    let curl = root.path().join("blocking-curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\ncat >/dev/null\n: > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nprintf '{{\"five_hour\":{{\"utilization\":99.0}}}}\\n200'\n",
            started.display(),
            release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();

    let forwarded = Arc::new(AtomicBool::new(false));
    let saw_forward = Arc::clone(&forwarded);
    let upstream = ControlledUpstream::start(move |request| {
        saw_forward.store(true, Ordering::SeqCst);
        request
            .respond(tiny_http::Response::from_string("{\"ok\":true}"))
            .ok();
    });
    let curl_value = curl.to_string_lossy().into_owned();
    let (proxy, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &["--auto", "--threshold", "0.98"],
        &[("SWAPDEX_CURL", curl_value.as_str())],
    );
    let proxy = ReapedChild::new(proxy);
    let request = std::thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("authorization", "Bearer CLIENT-TOKEN")
            .header("content-type", "application/json")
            .send(b"{\"turn\":1}".as_slice())
            .expect("proxy answered");
        let status = response.status().as_u16();
        let mut body = String::new();
        response
            .body_mut()
            .as_reader()
            .read_to_string(&mut body)
            .unwrap();
        (status, body)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !started.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !started.exists() {
        std::fs::write(&release, b"release").ok();
        let _ = request.join();
        proxy.stop();
        upstream.close();
        panic!("the quota fixture never blocked account selection");
    }

    swapdex::slots::Slots::open(&swapdex::paths::Paths::rooted(root.path()))
        .unwrap()
        .set_serving_off()
        .unwrap();
    std::fs::write(&release, b"release").unwrap();
    let (status, response_body) = request.join().unwrap();
    let output = proxy.stop_with_stdout();
    upstream.close();

    assert_eq!(status, 502, "{output}");
    assert!(!forwarded.load(Ordering::SeqCst), "{output}");
    assert!(
        response_body.contains("serving changed to passthrough"),
        "{response_body}"
    );
}

/// When swapdex has no usable login to offer, it must get out of the way: the
/// turn goes upstream with the CLIENT's own Authorization, which is what Claude
/// would have sent with no proxy at all. Being unable to help is not a reason to
/// break the tool.
#[test]
fn an_unusable_selected_account_does_not_send_the_clients_other_login() {
    let root = tempfile::tempdir().unwrap();
    // A slot in the registry whose credential is unreadable - and a second one
    // that IS readable, because a proxy able to read nothing at all now refuses
    // to start rather than answer every turn with the client's own login and
    // never say so. The account under test is the one the pointer names.
    seed_slot(root.path(), "healthy", "bbbb2222", "AT-HEALTHY", false);
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots").join("aaaa1111");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(slot.join(".credentials.json"), b"not json").unwrap();
    let mut recs: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(store.join("slots.json")).unwrap()).unwrap();
    recs.push(serde_json::json!({
        "name": "broken", "id": "aaaa1111", "config_dir": slot, "adopted": false
    }));
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&recs).unwrap(),
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

    assert!(body.contains("swapdex_proxy_error"), "{body}");
    assert!(
        auths(&sink).is_empty(),
        "no credential was authorized to serve"
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
        br#"{"claudeAiOauth":{"accessToken":"AT-STALE","expiresAt":1}}"#,
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

    assert!(body.contains("swapdex_proxy_error"), "{body}");
    let seen = auths(&sink);
    assert!(
        !seen.iter().any(|a| a.contains("AT-STALE")),
        "the lapsed token was never sent: {seen:?}"
    );
    assert!(
        seen.is_empty(),
        "no implicit client-account fallback: {seen:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_stale_claude_slot_uses_its_verified_native_login_without_copying_or_refreshing() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "selected", "selected-slot", "OLD-ACCESS", true);
    let slot = root.path().join(".local/share/swapdex/slots/selected-slot");
    let native = root.path().join(".claude");
    std::fs::create_dir_all(&native).unwrap();
    let identity = br#"{"oauthAccount":{"accountUuid":"same-user","organizationUuid":"same-org"}}"#;
    std::fs::write(slot.join(".claude.json"), identity).unwrap();
    std::fs::write(root.path().join(".claude.json"), identity).unwrap();
    // The default CLI's identity lives beside .claude, not inside it.
    let old = br#"{"claudeAiOauth":{"accessToken":"OLD-ACCESS","refreshToken":"OLD-REFRESH","expiresAt":1}}"#;
    let live = br#"{"claudeAiOauth":{"accessToken":"NATIVE-ACCESS","refreshToken":"NATIVE-REFRESH","expiresAt":9999999999999}}"#;
    std::fs::write(slot.join(".credentials.json"), old).unwrap();
    std::fs::write(native.join(".credentials.json"), live).unwrap();

    let fake_cli = root.path().join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &fake_cli).unwrap();
    let cli = Command::new(&fake_cli)
        .arg("120")
        .env("HOME", root.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .spawn()
        .unwrap();
    let pid = cli.id();
    let _cli = ReapedChild::new(cli);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim()
        != "claude"
    {
        assert!(
            std::time::Instant::now() < deadline,
            "fake native process did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&calls);
    let upstream = ControlledUpstream::start(move |rq| {
        let auth = rq
            .headers()
            .iter()
            .find(|h| h.field.equiv("authorization"))
            .map(|h| h.value.as_str().to_owned())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth);
        rq.respond(tiny_http::Response::from_string("{\"ok\":true}"))
            .unwrap();
    });
    let curl = root.path().join("curl-no-oauth");
    std::fs::write(
        &curl,
        r#"#!/bin/sh
config=$(cat)
case "$config" in
  *'/oauth/token'*) printf x >> "$SWAPDEX_ROOT/oauth-calls" ;;
esac
printf '%s\n' '{}' '400'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (proxy, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &[],
        &[("SWAPDEX_CURL", curl.to_str().unwrap())],
    );
    let proxy = ReapedChild::new(proxy);
    assert!(post_through(port, "{}").contains("\"ok\":true"));
    assert_eq!(*calls.lock().unwrap(), vec!["Bearer NATIVE-ACCESS"]);
    assert_eq!(std::fs::read(slot.join(".credentials.json")).unwrap(), old);
    assert_eq!(
        std::fs::read(native.join(".credentials.json")).unwrap(),
        live
    );
    assert!(!root.path().join("oauth-calls").exists());

    let list = Command::new(bin())
        .args(["ls", "--json"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert!(list.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "selected")
        .unwrap();
    assert!(
        !row["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("expired"),
        "{row}"
    );
    assert_eq!(row["renewal_owner"]["claude-code"], "native");
    proxy.stop();
    upstream.close();
}

/// Exhausting the managed accounts must surface failure without silently billing
/// the unrelated login the client supplied.
#[test]
fn all_accounts_refused_does_not_authorize_the_clients_own_login() {
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

    assert!(!body.contains("\"ok\":true"), "{body}");
    assert!(
        !auths(&sink).contains(&"Bearer CLIENT-TOKEN".to_string()),
        "unselected client login was sent: {:?}",
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
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
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
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .env("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
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
        .env("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
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
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .env("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .env("SWAPDEX_CURL", "/bin/false")
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

fn seed_signal_test_account(root: &std::path::Path, tool: &str) {
    if tool == "codex" {
        seed_codex_slot(root, "work", "codex-work", "AT-WORK", "acct-work", true);
    } else {
        seed_slot(root, "work", "claude-work", "AT-WORK", true);
    }
}

fn start_signal_test_proxy(
    root: &std::path::Path,
    tool: &str,
    upstream: &str,
) -> (ReapedChild, u16) {
    let mut command = Command::new(bin());
    command
        .args(["proxy", "--port", "0"])
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .env("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .env("SWAPDEX_CURL", "/bin/false")
        // Keep stdout open without relying on a blocking read for readiness.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if tool == "codex" {
        command
            .args(["--tool", "codex"])
            .env("SWAPDEX_UPSTREAM_CODEX", upstream);
    } else {
        command.env("SWAPDEX_UPSTREAM", upstream);
    }
    let child = command.spawn().unwrap();
    let pid = child.id();
    let mut proxy = ReapedChild::new(child);
    let paths = swapdex::paths::Paths::rooted(root);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(status) = proxy.0.as_mut().unwrap().try_wait().unwrap() {
            panic!("{tool} proxy exited during startup: {status}");
        }
        if let Some((marker_pid, port, build)) = swapdex::proxy::running_proxy_for(&paths, tool) {
            assert_eq!(marker_pid, pid as i32, "proxy marker named another process");
            assert!(!build.is_empty(), "proxy marker omitted its build identity");
            return (proxy, port);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{tool} proxy did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn post_signal_test_turn(tool: &str, port: u16) -> u16 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(3)))
        .build()
        .into();
    let (path, body) = if tool == "codex" {
        ("/v1/responses", r#"{"input":[]}"#)
    } else {
        ("/v1/messages", r#"{"turn":1}"#)
    };
    let mut request = agent
        .post(format!("http://127.0.0.1:{port}{path}"))
        .header("authorization", "Bearer CLIENT-TOKEN")
        .header("content-type", "application/json");
    if tool == "codex" {
        request = request.header("chatgpt-account-id", "acct-client");
    }
    let mut response = request
        .send(body.as_bytes())
        .expect("proxy answered within the test deadline");
    let status = response.status().as_u16();
    let mut response_body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut response_body)
        .expect("proxy response completed within the test deadline");
    status
}

fn assert_proxy_stays_running(
    proxy: &mut ReapedChild,
    duration: std::time::Duration,
    context: &str,
) {
    let deadline = std::time::Instant::now() + duration;
    loop {
        let status = proxy.0.as_mut().unwrap().try_wait().unwrap();
        assert!(
            status.is_none(),
            "proxy exited {context}: {}",
            status.unwrap()
        );
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn executable_proxy_ignores_sigpipe_and_keeps_serving(tool: &str) {
    let root = tempfile::tempdir().unwrap();
    seed_signal_test_account(root.path(), tool);
    let upstream = ControlledUpstream::start(|mut request| {
        let mut body = Vec::new();
        request.as_reader().read_to_end(&mut body).ok();
        request
            .respond(tiny_http::Response::from_string(r#"{"ok":true}"#))
            .ok();
    });
    let (mut proxy, port) = start_signal_test_proxy(root.path(), tool, upstream.url());
    let pid = proxy.0.as_ref().unwrap().id();

    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGPIPE) };
    assert_eq!(sent, 0, "could not send SIGPIPE to the {tool} proxy");
    assert_proxy_stays_running(
        &mut proxy,
        std::time::Duration::from_millis(500),
        "after SIGPIPE",
    );
    assert_eq!(
        post_signal_test_turn(tool, port),
        200,
        "the same {tool} proxy did not answer after SIGPIPE"
    );
    assert_eq!(proxy.0.as_ref().unwrap().id(), pid);

    proxy.stop();
    upstream.close();
}

#[test]
fn executable_claude_proxy_ignores_sigpipe_and_keeps_serving() {
    executable_proxy_ignores_sigpipe_and_keeps_serving("claude-code");
}

#[test]
fn executable_codex_proxy_ignores_sigpipe_and_keeps_serving() {
    executable_proxy_ignores_sigpipe_and_keeps_serving("codex");
}

fn open_turn_for_disconnect(port: u16, tool: &str) -> std::net::TcpStream {
    let (path, body, identity) = if tool == "codex" {
        (
            "/v1/responses",
            r#"{"input":[]}"#,
            "Authorization: Bearer CLIENT-TOKEN\r\nChatGPT-Account-ID: acct-client\r\n",
        )
    } else {
        (
            "/v1/messages",
            r#"{"turn":1}"#,
            "Authorization: Bearer CLIENT-TOKEN\r\n",
        )
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{identity}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut client =
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(3)).unwrap();
    client
        .set_write_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    client.write_all(request.as_bytes()).unwrap();
    client
}

fn reset_client_connection(client: std::net::TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    let set = unsafe {
        libc::setsockopt(
            client.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(set, 0, "could not configure a reset-on-close client");
    drop(client);
}

fn executable_proxy_survives_a_disconnected_client(tool: &str) {
    let root = tempfile::tempdir().unwrap();
    seed_signal_test_account(root.path(), tool);
    let (received_tx, received_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (responded_tx, responded_rx) = std::sync::mpsc::channel();
    let mut first = true;
    let upstream = ControlledUpstream::start(move |mut request| {
        let mut body = Vec::new();
        request.as_reader().read_to_end(&mut body).ok();
        let was_first = std::mem::replace(&mut first, false);
        let released = if was_first {
            received_tx.send(()).is_ok()
                && release_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .is_ok()
        } else {
            true
        };
        let answered = released
            && request
                .respond(tiny_http::Response::from_string(r#"{"ok":true}"#))
                .is_ok();
        if was_first {
            responded_tx.send(answered).ok();
        }
    });
    let (mut proxy, port) = start_signal_test_proxy(root.path(), tool, upstream.url());
    let pid = proxy.0.as_ref().unwrap().id();

    let client = open_turn_for_disconnect(port, tool);
    received_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("the upstream did not receive the abandoned turn");
    reset_client_connection(client);
    release_tx.send(()).unwrap();
    assert!(
        responded_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("the upstream did not finish the abandoned turn"),
        "the upstream could not answer the abandoned turn"
    );
    assert_proxy_stays_running(
        &mut proxy,
        std::time::Duration::from_millis(500),
        "after a client disconnected before its response",
    );
    assert_eq!(
        post_signal_test_turn(tool, port),
        200,
        "the same {tool} proxy did not answer after a client disconnected"
    );
    assert_eq!(proxy.0.as_ref().unwrap().id(), pid);

    proxy.stop();
    upstream.close();
}

#[test]
fn executable_claude_proxy_survives_a_disconnected_client() {
    executable_proxy_survives_a_disconnected_client("claude-code");
}

#[test]
fn executable_codex_proxy_survives_a_disconnected_client() {
    executable_proxy_survives_a_disconnected_client("codex");
}

fn select_serving(root: &std::path::Path, tool: &str, name: &str) {
    let paths = swapdex::paths::Paths::rooted(root);
    swapdex::slots::Slots::open_for(&paths, tool)
        .unwrap()
        .set_serving(name)
        .unwrap();
}

/// A Codex proxy must never search Claude's registry when its selected account
/// is already benched. That produces neither a valid fallback nor a useful
/// error; it attempts to load a Claude home as a Codex login.
#[test]
fn codex_preemptive_fallback_stays_in_the_codex_registry() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(
        root.path(),
        "claude-only",
        "claude-only-id",
        "AT-CLAUDE",
        true,
    );
    seed_codex_slot(root.path(), "a", "codex-a-id", "AT-A", "acct-a", true);
    seed_codex_slot(root.path(), "b", "codex-b-id", "AT-B", "acct-b", false);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream_refusing(Arc::clone(&seen), "AT-A", 401);
    let (proxy, port) = start_codex_proxy(root.path(), &upstream, &["--auto"]);
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_codex_turn(port).0, 200, "A refused, then B served");
    select_serving(root.path(), "codex", "a");
    assert_eq!(
        post_codex_turn(port).0,
        200,
        "a known-benched Codex selection should fall back within Codex"
    );

    proxy.stop();
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|(auth, account)| auth != "Bearer AT-CLAUDE" && account.starts_with("acct-")),
        "Codex fallback crossed into a Claude slot: {seen:?}"
    );
    assert_eq!(
        seen,
        vec![
            ("Bearer AT-A".into(), "acct-a".into()),
            ("Bearer AT-B".into(), "acct-b".into()),
            ("Bearer AT-B".into(), "acct-b".into()),
        ]
    );
}

fn set_codex_id_token(root: &std::path::Path, id: &str, id_token: Option<String>) {
    let auth = root
        .join(".local/share/swapdex/slots")
        .join(id)
        .join("auth.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&auth).unwrap()).unwrap();
    let tokens = value["tokens"].as_object_mut().unwrap();
    match id_token {
        Some(token) => {
            tokens.insert("id_token".into(), token.into());
        }
        None => {
            tokens.remove("id_token");
        }
    }
    std::fs::write(auth, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn controlled_codex_upstream_refusing(
    sink: Arc<Mutex<Vec<(String, String)>>>,
    refuse_token: &str,
) -> ControlledUpstream {
    let refuse_token = refuse_token.to_string();
    ControlledUpstream::start(move |mut request| {
        let header = |name: &'static str| {
            request
                .headers()
                .iter()
                .find(|header| header.field.equiv(name))
                .map(|header| header.value.as_str().to_string())
                .unwrap_or_default()
        };
        let auth = header("authorization");
        let account = header("chatgpt-account-id");
        let mut body = Vec::new();
        request.as_reader().read_to_end(&mut body).ok();
        sink.lock().unwrap().push((auth.clone(), account));
        let refused = auth == format!("Bearer {refuse_token}");
        request
            .respond(
                tiny_http::Response::from_string(if refused {
                    "{\"error\":\"spent\"}"
                } else {
                    "{\"ok\":true}"
                })
                .with_status_code(if refused { 429 } else { 200 }),
            )
            .ok();
    })
}

#[test]
fn codex_failover_skips_a_twin_subject_and_workspace() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "a", "id-a", "AT-A", "workspace", true);
    seed_codex_slot(
        root.path(),
        "a-twin",
        "id-a-twin",
        "AT-TWIN",
        "workspace",
        false,
    );
    seed_codex_slot(root.path(), "c", "id-c", "AT-C", "workspace-c", false);
    let subject = codex_id_token("subject-a");
    set_codex_id_token(root.path(), "id-a", Some(subject.clone()));
    set_codex_id_token(root.path(), "id-a-twin", Some(subject));
    set_codex_id_token(root.path(), "id-c", Some(codex_id_token("subject-c")));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = controlled_codex_upstream_refusing(Arc::clone(&seen), "AT-A");
    let (proxy, port) = start_codex_proxy(root.path(), upstream.url(), &["--auto"]);
    let proxy = ReapedChild::new(proxy);
    assert_eq!(post_codex_turn(port).0, 200);
    proxy.stop();
    upstream.close();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("Bearer AT-A".into(), "workspace".into()),
            ("Bearer AT-C".into(), "workspace-c".into()),
        ],
        "the refused payer's twin must not receive the same turn"
    );
}

#[test]
fn codex_failover_keeps_distinct_subjects_in_one_workspace() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "a", "id-a", "AT-A", "workspace", true);
    seed_codex_slot(root.path(), "b", "id-b", "AT-B", "workspace", false);
    set_codex_id_token(root.path(), "id-a", Some(codex_id_token("subject-a")));
    set_codex_id_token(root.path(), "id-b", Some(codex_id_token("subject-b")));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = controlled_codex_upstream_refusing(Arc::clone(&seen), "AT-A");
    let (proxy, port) = start_codex_proxy(root.path(), upstream.url(), &["--auto"]);
    let proxy = ReapedChild::new(proxy);
    assert_eq!(post_codex_turn(port).0, 200);
    proxy.stop();
    upstream.close();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("Bearer AT-A".into(), "workspace".into()),
            ("Bearer AT-B".into(), "workspace".into()),
        ],
        "workspace membership alone must not merge distinct users"
    );
}

#[test]
fn codex_failover_uses_workspace_for_opaque_id_tokens() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(root.path(), "a", "id-a", "AT-A", "workspace", true);
    seed_codex_slot(
        root.path(),
        "a-twin",
        "id-a-twin",
        "AT-TWIN",
        "workspace",
        false,
    );
    seed_codex_slot(root.path(), "c", "id-c", "AT-C", "workspace-c", false);
    set_codex_id_token(root.path(), "id-a", Some("not-a-jwt".into()));
    set_codex_id_token(root.path(), "id-a-twin", None);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = controlled_codex_upstream_refusing(Arc::clone(&seen), "AT-A");
    let (proxy, port) = start_codex_proxy(root.path(), upstream.url(), &["--auto"]);
    let proxy = ReapedChild::new(proxy);
    assert_eq!(post_codex_turn(port).0, 200);
    proxy.stop();
    upstream.close();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("Bearer AT-A".into(), "workspace".into()),
            ("Bearer AT-C".into(), "workspace-c".into()),
        ],
        "opaque credentials keep the workspace fallback instead of retrying a twin"
    );
}

/// A refusal from an older in-flight request must not install a rotation after
/// a newer explicit serving choice, whether or not another turn has observed it.
/// The late request receives its own refusal; the selected account serves every
/// subsequent turn. Exercise both protocols and both auto policies.
#[test]
fn late_inflight_refusal_cannot_override_a_newer_human_selection() {
    for tool in ["claude-code", "codex"] {
        for auto in [false, true] {
            for choice_observed_by_request in [false, true] {
                let root = tempfile::tempdir().unwrap();
                if tool == "codex" {
                    for (name, id, token, account, default) in [
                        ("a", "codex-a", "AT-A", "acct-a", true),
                        ("b", "codex-b", "AT-B", "acct-b", false),
                        ("c", "codex-c", "AT-C", "acct-c", false),
                    ] {
                        seed_codex_slot(root.path(), name, id, token, account, default);
                    }
                } else {
                    for (name, id, token, default) in [
                        ("a", "claude-a", "AT-A", true),
                        ("b", "claude-b", "AT-B", false),
                        ("c", "claude-c", "AT-C", false),
                    ] {
                        seed_slot(root.path(), name, id, token, default);
                    }
                }

                let seen = Arc::new(Mutex::new(Vec::new()));
                let sink = Arc::clone(&seen);
                let first_a = Arc::new(AtomicBool::new(true));
                let reject_a = Arc::clone(&first_a);
                let started = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
                let started_by_upstream = Arc::clone(&started);
                let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
                let release_upstream = Arc::clone(&release);
                let upstream = ControlledUpstream::start_concurrent(move |mut request| {
                    let auth = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("authorization"))
                        .map(|header| header.value.as_str().to_string())
                        .unwrap_or_default();
                    let mut body = Vec::new();
                    request.as_reader().read_to_end(&mut body).ok();
                    sink.lock().unwrap().push(auth.clone());
                    let late = auth == "Bearer AT-A" && reject_a.swap(false, Ordering::SeqCst);
                    if late {
                        let (ready, wake) = &*started_by_upstream;
                        *ready.lock().unwrap() = true;
                        wake.notify_all();
                        let (go, wake) = &*release_upstream;
                        let guard = go.lock().unwrap();
                        let _ = wake
                            .wait_timeout_while(guard, std::time::Duration::from_secs(5), |go| !*go)
                            .unwrap();
                    }
                    let status: u16 = if late { 401 } else { 200 };
                    request
                        .respond(
                            tiny_http::Response::from_string(if late {
                                "{\"error\":\"rejected A\"}"
                            } else {
                                "{\"ok\":true}"
                            })
                            .with_status_code(status),
                        )
                        .ok();
                });
                let extra = if auto {
                    vec!["--auto"]
                } else {
                    vec!["--no-auto"]
                };
                let (proxy, port) = if tool == "codex" {
                    start_codex_proxy(root.path(), upstream.url(), &extra)
                } else {
                    start_proxy_with_env(
                        root.path(),
                        upstream.url(),
                        &extra,
                        &[("SWAPDEX_CURL", "/bin/false")],
                    )
                };
                let proxy = ReapedChild::new(proxy);

                let request_one = std::thread::spawn(move || {
                    if tool == "codex" {
                        post_codex_turn(port).0
                    } else {
                        post_through_status(port, "{\"turn\":1}")
                    }
                });
                let (ready, wake) = &*started;
                let guard = ready.lock().unwrap();
                let (guard, timeout) = wake
                    .wait_timeout_while(guard, std::time::Duration::from_secs(5), |ready| !*ready)
                    .unwrap();
                assert!(*guard && !timeout.timed_out(), "A never reached upstream");
                drop(guard);

                select_serving(root.path(), tool, "c");
                if choice_observed_by_request {
                    let second = if tool == "codex" {
                        post_codex_turn(port).0
                    } else {
                        post_through_status(port, "{\"turn\":2}")
                    };
                    assert_eq!(second, 200, "new human choice C should serve immediately");
                }
                let (go, wake) = &*release;
                *go.lock().unwrap() = true;
                wake.notify_all();
                let first = request_one.join().unwrap();
                let third = if tool == "codex" {
                    post_codex_turn(port).0
                } else {
                    post_through_status(port, "{\"turn\":3}")
                };
                let output = proxy.stop_with_stdout();
                upstream.close();
                let seen = seen.lock().unwrap().clone();
                assert_eq!(
                    first, 401,
                    "tool={tool} auto={auto} preobserved={choice_observed_by_request}: late A did not keep its own result; seen={seen:?}\n{output}"
                );
                assert_eq!(
                    third, 200,
                    "tool={tool} auto={auto} preobserved={choice_observed_by_request}: {output}"
                );
                let mut expected = vec!["Bearer AT-A".to_string()];
                if choice_observed_by_request {
                    expected.push("Bearer AT-C".to_string());
                }
                expected.push("Bearer AT-C".to_string());
                assert_eq!(
                    seen,
                    expected,
                    "tool={tool} auto={auto} preobserved={choice_observed_by_request}: a late refusal displaced C"
                );
            }
        }
    }
}

/// Once an upstream has fully accepted a body, a TCP reset does not reveal
/// whether the operation ran. Replaying that POST can duplicate a paid turn.
#[test]
fn accepted_post_is_not_replayed_after_an_ambiguous_transport_failure() {
    for tool in ["claude-code", "codex"] {
        let root = tempfile::tempdir().unwrap();
        if tool == "codex" {
            seed_codex_slot(root.path(), "a", "codex-a", "AT-A", "acct-a", true);
        } else {
            seed_slot(root.path(), "a", "claude-a", "AT-A", true);
        }
        let upstream = ResetAfterReadUpstream::start();
        let (proxy, port) = if tool == "codex" {
            start_codex_proxy(root.path(), upstream.url(), &[])
        } else {
            start_proxy(root.path(), upstream.url(), &[])
        };
        let proxy = ReapedChild::new(proxy);
        let status = if tool == "codex" {
            post_codex_turn(port).0
        } else {
            post_through_status(port, "{\"turn\":1}")
        };
        assert_eq!(status, 502, "an ambiguous {tool} result must surface");
        proxy.stop();
        let seen = upstream.seen();
        upstream.close();
        assert_eq!(
            seen.len(),
            1,
            "the accepted {tool} POST was replayed: {seen:?}"
        );
        assert_eq!(seen[0].0, "POST");
        assert!(!seen[0].1.is_empty());
    }
}

#[test]
fn bodyless_get_still_retries_after_a_transport_reset() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "a", "claude-a", "AT-A", true);
    let upstream = ResetAfterReadUpstream::start();
    let (proxy, port) = start_proxy(root.path(), upstream.url(), &[]);
    let proxy = ReapedChild::new(proxy);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .get(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("authorization", "Bearer CLIENT-TOKEN")
        .call()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    proxy.stop();
    let seen = upstream.seen();
    upstream.close();
    assert_eq!(seen.len(), 2, "the safe GET retry was removed: {seen:?}");
    assert!(seen
        .iter()
        .all(|(method, body)| method == "GET" && body.is_empty()));
}

/// Codex carries account identity in a header instead of Claude's body field.
/// Explicit off must preserve both client headers, beat `--account`, survive a
/// restart, and return one upstream failure without rotating or retrying.
#[test]
fn codex_serve_off_is_durable_passthrough_and_beats_a_pin() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "cccc1111",
        "AT-WORK",
        "acct-work",
        true,
    );
    let off = Command::new(bin())
        .args(["serve", "--off", "--tool", "codex"])
        .env("SWAPDEX_ROOT", root.path())
        .output()
        .unwrap();
    assert!(off.status.success(), "serve --off failed: {off:?}");

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream_refusing(sink.clone(), "", 403);
    for _ in 0..2 {
        let (mut proxy, port) =
            start_codex_proxy(root.path(), &upstream, &["--auto", "--account", "work"]);
        let (status, _) = post_codex_turn(port);
        assert_eq!(status, 403, "the client's upstream failure was replaced");
        proxy.kill().ok();
        proxy.wait().ok();
    }

    let marker = root.path().join(".local/share/swapdex/serving-codex");
    assert!(marker.exists(), "off did not survive the proxy restart");
    let seen = sink.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "a failed passthrough was retried: {seen:?}");
    assert!(
        seen.iter()
            .all(|(auth, account)| auth == "Bearer CLIENT-TOKEN" && account == "acct-client"),
        "managed Codex identity overrode the client: {seen:?}"
    );
}

/// Every command surface that labels a payer must distinguish explicit off
/// from legacy pointer absence. The default is still where sessions launch,
/// but it must not be presented as paying while passthrough is selected.
#[test]
fn cli_payer_labels_do_not_claim_the_default_while_off() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "cccc1111",
        "AT-WORK",
        "acct-work",
        true,
    );
    let run = |args: &[&str]| {
        let out = Command::new(bin())
            .args(args)
            .env("SWAPDEX_ROOT", root.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    run(&["serve", "--off", "--tool", "codex"]);

    let list = run(&["ls"]);
    assert!(
        !list.contains("<- pays"),
        "ls claimed a managed account pays while off:\n{list}"
    );
    let status = run(&["status", "--short"]);
    assert!(
        !status.contains("codex:work"),
        "status claimed the default pays while off:\n{status}"
    );
    let serving = run(&["serve", "--tool", "codex"]);
    assert!(
        serving.contains("passthrough") || serving.contains("off"),
        "serve did not report the durable off state:\n{serving}"
    );
    let quiet = run(&["serve", "--quiet", "--tool", "codex"]);
    assert!(
        !quiet.contains("work"),
        "the payer label named the default while off:\n{quiet}"
    );
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

/// Environment variables must not override the shim's routing decision.
mod codex_routing_ignores_inherited_shell_state {
    use std::sync::{Mutex, MutexGuard};
    use swapdex::shim::codex_shim_script;

    static FIXTURE_EXEC_LOCK: Mutex<()> = Mutex::new(());

    fn fixture_exec_lock() -> MutexGuard<'static, ()> {
        FIXTURE_EXEC_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[derive(Debug, Eq, PartialEq)]
    struct Launch {
        home: String,
        args: Vec<String>,
    }

    /// Run the generated shim with stubs and return exactly what the real tool
    /// receives. `env` arrives the way a caller's exported variables would.
    fn tool_receives(nonce: &str, env: &[(&str, &str)], args: &[&str]) -> Launch {
        let _exec_guard = fixture_exec_lock();
        let prefix = format!("sx-guard-{nonce}-");
        let tmp = tempfile::Builder::new().prefix(&prefix).tempdir().unwrap();
        let root = tmp.path();
        let pointer = root.join("ptr");
        let pointed_home = root.join("pointed-home");
        std::fs::write(&pointer, pointed_home.to_string_lossy().as_bytes()).unwrap();
        let sx = root.join("swapdex");
        std::fs::write(
            &sx,
            "#!/bin/sh\nfor a in \"$@\"; do\n\tcase \"$a\" in\n\t--ensure) echo 8788; exit 0 ;;\n\tserve) shift; printf '%s' work; exit 0 ;;\n\tesac\ndone\nexit 0\n",
        )
        .unwrap();
        let tool = root.join("tool");
        std::fs::write(
            &tool,
            "#!/bin/sh\nprintf 'home=%s\\n' \"$CODEX_HOME\"\nfor a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done\n",
        )
        .unwrap();
        let shim = root.join("shim");
        std::fs::write(&shim, codex_shim_script(&pointer, &tool, &sx)).unwrap();
        for f in [&sx, &tool, &shim] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cmd = std::process::Command::new("sh");
        cmd.arg(&shim).args(args);
        cmd.env_remove("CODEX_HOME");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "shim failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let mut lines = stdout.lines();
        let home = lines
            .next()
            .and_then(|line| line.strip_prefix("home="))
            .expect("tool reported CODEX_HOME")
            .to_owned();
        let args = lines
            .map(|line| {
                line.strip_prefix("arg=")
                    .expect("tool reported one argument per line")
                    .to_owned()
            })
            .collect();
        Launch { home, args }
    }

    fn managed_args(original: &[&str]) -> Vec<String> {
        ["-c", "openai_base_url=http://127.0.0.1:8788/v1"]
            .into_iter()
            .chain(original.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn resume_uses_the_reported_proxy_without_changing_provider() {
        let got = tool_receives("resume", &[("port", "3000")], &["resume"]);
        assert!(!got.args.iter().any(|arg| arg.contains("model_provider")));
        assert_eq!(got.args, managed_args(&["resume"]));
    }

    #[test]
    fn an_inherited_port_does_not_reopen_the_override_on_login() {
        let got = tool_receives("login", &[("port", "3000")], &["login"]);
        assert!(
            !got.args.iter().any(|arg| arg.contains("model_provider")),
            "a sign-in must reach the real backend, got: {got:?}"
        );
    }

    /// The other direction: a turn that DOES talk to the model still gets the
    /// override, so the fix cannot be "never apply it".
    #[test]
    fn a_talking_turn_still_gets_the_override() {
        let got = tool_receives("talk", &[("port", "3000")], &["hello"]);
        assert!(
            got.args
                .iter()
                .any(|arg| arg == "openai_base_url=http://127.0.0.1:8788/v1"),
            "a turn routes through the proxy swapdex reported, got: {got:?}"
        );
    }

    #[test]
    fn profile_forms_stay_managed_and_preserve_arguments_and_home() {
        for (nonce, profile) in [
            ("profile-short", vec!["-p", "worker"]),
            ("profile-long", vec!["--profile", "worker"]),
            ("profile-short-attached", vec!["-pworker"]),
            ("profile-long-attached", vec!["--profile=worker"]),
        ] {
            let mut original = vec!["--strict-config", "exec"];
            original.extend(profile);
            original.extend(["resume", "thread-42"]);
            let got = tool_receives(nonce, &[("CODEX_HOME", "/keep/codex-home")], &original);
            assert_eq!(got.home, "/keep/codex-home", "profile form: {original:?}");
            assert_eq!(
                got.args,
                managed_args(&original),
                "profile form: {original:?}"
            );
        }
    }

    #[test]
    fn a_profile_value_named_login_is_not_an_auth_command() {
        let original = ["-p", "login", "exec", "hello"];
        let got = tool_receives("profile-named-login", &[], &original);
        assert_eq!(got.args, managed_args(&original));
    }

    #[test]
    fn explicit_provider_config_with_a_profile_still_bypasses_routing() {
        let original = [
            "--profile=worker",
            "exec",
            "-c",
            "model_provider=company",
            "hello",
        ];
        let got = tool_receives("profile-explicit-provider", &[], &original);
        assert_eq!(got.args, original.map(str::to_owned));
    }

    #[test]
    fn auth_commands_with_a_profile_still_bypass_routing() {
        for (nonce, command) in [
            ("profile-auth-login", "login"),
            ("profile-auth-logout", "logout"),
        ] {
            let original = ["--profile=worker", command];
            let got = tool_receives(nonce, &[], &original);
            assert_eq!(got.args, original.map(str::to_owned));
        }
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
            let mut s = Slots::open_for_update(&paths, "codex").unwrap();
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
            let mut s = Slots::open_for_update(&paths, "codex").unwrap();
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
            let mut s = Slots::open_for_update(&paths, "codex").unwrap();
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
                let mut s = Slots::open_for_update(&paths, tool).unwrap();
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
        Slots::open_for_update(&paths, "codex")
            .unwrap()
            .create("work")
            .unwrap();
        assert!(has_any_account(&paths), "a slot counts");
    }

    #[test]
    fn and_so_is_a_claude_one() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        Slots::open_for_update(&paths, "claude-code")
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
        let mut s = Slots::open_for_update(&paths, "codex").unwrap();
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

    /// Off is a durable state and a timeline event, not deletion of the serving
    /// pointer. Its event contract is consumed by sessionwiki too, so both the
    /// action spelling and empty account are pinned here. Naming an account
    /// afterwards intentionally re-enables managed serving.
    #[test]
    fn serve_off_records_unknown_payer_and_serve_name_reenables() {
        let root = tempfile::tempdir().unwrap();
        let paths = store_with_two_codex_accounts(root.path());
        let slots = Slots::open_for(&paths, "codex").unwrap();
        slots.set_default("home").unwrap();

        commands::serve(&paths, None, true, Some(ToolSel::Codex), false).unwrap();
        let events = read_timeline(&paths);
        let off = events.last().expect("serve --off appended an event");
        assert_eq!(off.action, "serve-off");
        assert_eq!(off.account, "");
        assert_eq!(off.tool, "codex");
        assert_eq!(payer_at(&events, "codex", LATER), None);
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().payer(),
            None,
            "explicit off must not fall through to the default"
        );
        assert_eq!(commands::payer_label(&paths, "codex"), None);
        assert_eq!(commands::active_slot_name(&paths, "codex"), None);

        commands::use_account(&paths, "home", Some(ToolSel::Codex), false, false).unwrap();
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().payer().as_deref(),
            Some("home"),
            "use intentionally re-enables the selected default"
        );
        commands::use_account(&paths, "payer", Some(ToolSel::Codex), false, false).unwrap();
        commands::serve(&paths, None, true, Some(ToolSel::Codex), false).unwrap();
        commands::restore(&paths, Some(ToolSel::Codex), false).unwrap();
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().payer().as_deref(),
            Some("home"),
            "restore intentionally re-enables the restored default"
        );

        commands::serve(&paths, None, true, Some(ToolSel::Codex), false).unwrap();
        commands::serve(&paths, Some("payer"), false, Some(ToolSel::Codex), false).unwrap();
        assert_eq!(
            Slots::open_for(&paths, "codex").unwrap().payer().as_deref(),
            Some("payer")
        );
        assert_eq!(
            payer_at(&read_timeline(&paths), "codex", LATER).as_deref(),
            Some("payer")
        );
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
        let rec = Slots::open_for_update(&paths, "codex")
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
    // Registered, pointed at, and never signed into - beside one that IS signed
    // in, so the proxy has something to serve with and starts at all.
    seed_slot(root.path(), "healthy", "bbbb2222", "AT-HEALTHY", false);
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots").join("cccc3333");
    std::fs::create_dir_all(&slot).unwrap();
    let mut recs: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(store.join("slots.json")).unwrap()).unwrap();
    recs.push(serde_json::json!({
        "name": "nologin", "id": "cccc3333", "config_dir": slot, "adopted": false
    }));
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&recs).unwrap(),
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
        let rec = Slots::open_for_update(&paths, "codex")
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
        let mut s = Slots::open_for_update(&paths, "codex").unwrap();
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
            let mut s = Slots::open_for_update(&paths, "claude-code").unwrap();
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
            let mut s = Slots::open_for_update(&paths, "claude-code").unwrap();
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
            pick_active(n("rnd"), n("bsgong"), n("bsgong"), false),
            n("rnd"),
            "the instruction wins even though the proxy has not caught up"
        );
        assert_eq!(
            pick_active(None, n("spare"), n("bsgong"), false),
            n("spare"),
            "nobody asked, so a rotation is the only thing that knows"
        );
        assert_eq!(
            pick_active(None, None, n("bsgong"), false),
            n("bsgong"),
            "and otherwise, where sessions start"
        );
        assert_eq!(pick_active(None, None, None, false), None);
        // ...but once the proxy has served someone else since the ask, the ask
        // is demonstrably not being honoured and reality is what to show.
        assert_eq!(
            pick_active(n("rnd"), n("bsgong"), n("bsgong"), true),
            n("bsgong"),
            "an ask the proxy has already refused is not what is happening"
        );
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

/// A proxy that can read no credential still binds the port, still answers, and
/// forwards the CLIENT's own login on every turn - looking like it works while
/// doing nothing it exists to do. Started from an ssh session with a locked
/// Keychain, that state served for a full day before anyone noticed.
///
/// Refusing is the better failure: the shim asks for a port, gets none, and the
/// tool runs with no proxy - which is the login the user already has.
#[test]
fn a_proxy_with_nothing_to_serve_with_refuses_to_start() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join(".local/share/swapdex");
    let slot = store.join("slots").join("aaaa1111");
    std::fs::create_dir_all(&slot).unwrap();
    // Registered, never signed into.
    std::fs::write(
        store.join("slots.json"),
        serde_json::to_vec_pretty(&serde_json::json!([{
            "name": "nologin", "id": "aaaa1111", "config_dir": slot, "adopted": false
        }]))
        .unwrap(),
    )
    .unwrap();

    // Spawned rather than run to completion: a proxy that DOES start never
    // returns, so `output()` would hang instead of failing - and a test that
    // hangs when the behaviour regresses is a test nobody reads.
    let mut child = Command::new(bin())
        .args(["proxy", "--port", "0"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_UPSTREAM", "http://127.0.0.1:9")
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let exited = (0..40).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        matches!(child.try_wait(), Ok(Some(_)))
    });
    if !exited {
        child.kill().ok();
        child.wait().ok();
        panic!("the proxy started even though it can read no credential");
    }
    let out = child.wait_with_output().unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "it must not start: {said}");
    assert!(
        said.contains("swapdex run"),
        "and it says what to do about it: {said}"
    );
    assert!(
        !store.join("proxy").exists(),
        "no marker either - nothing should think a proxy is up"
    );
}

/// An upstream that reports every account as nearly spent and echoes back the
/// model it was asked for.
fn upstream_reporting_full(sink: Arc<Mutex<Vec<String>>>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut rq in server.incoming_requests() {
            let url = rq.url().to_string();
            let mut body = Vec::new();
            rq.as_reader().read_to_end(&mut body).ok();
            if url.contains("usage") {
                let _ = rq.respond(tiny_http::Response::from_string(
                    r#"{"five_hour":{"utilization":99.0},"seven_day":{"utilization":99.0}}"#,
                ));
                continue;
            }
            let model = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["model"].as_str().map(str::to_string))
                .unwrap_or_default();
            let fallback = model == "claude-sonnet-5";
            sink.lock().unwrap().push(model);
            // Refuse everything except the fallback: that is what "every account
            // is out" looks like from here, and it needs no usage endpoint.
            let resp = if fallback {
                tiny_http::Response::from_string("{\"ok\":true}").with_status_code(200)
            } else {
                tiny_http::Response::from_string("{\"error\":\"spent\"}").with_status_code(429)
            };
            let _ = rq.respond(resp);
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Changing the model gives the user something other than what they asked for,
/// so it is the LAST thing swapdex does: only once every account is past the
/// threshold and there is nowhere left to rotate. With room anywhere, the model
/// they asked for goes through untouched.
#[test]
fn the_fallback_model_is_asked_for_only_when_there_is_nowhere_left() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "one", "aaaa1111", "AT-ONE", true);
    seed_slot(root.path(), "two", "bbbb2222", "AT-TWO", false);
    std::fs::write(
        root.path().join(".local/share/swapdex/settings.json"),
        br#"{"fallback_model":"claude-sonnet-5","proxy_threshold":0.9}"#,
    )
    .unwrap();

    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = upstream_reporting_full(sink.clone());
    let (mut child, port) = start_proxy(root.path(), &upstream, &["--auto"]);

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    for _ in 0..2 {
        let _ = agent
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("authorization", "Bearer CLIENT-OWN")
            .send(r#"{"model":"claude-opus-5","messages":[]}"#);
    }
    child.kill().ok();
    child.wait().ok();

    let seen = sink.lock().unwrap().clone();
    assert!(
        seen.iter().any(|m| m == "claude-sonnet-5"),
        "with every account past the threshold it asked for the fallback: {seen:?}"
    );
}

/// A proxy started by the shim outlives its shell, gets reparented to launchd,
/// and keeps the port. The supervised agent then cannot bind, exits 1, and
/// KeepAlive restarts it into that same failure for as long as the machine is
/// on - 166 times on a real Mac before anyone looked at it. The new one takes
/// the port from the old instead.
#[test]
fn a_second_proxy_for_the_same_tool_takes_the_port_rather_than_failing() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "rnd", "aaaa1111", "AT-RND", true);
    let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let upstream = fake_upstream(sink.clone());

    let (mut first, port) = start_proxy(root.path(), &upstream, &[]);
    // The squatter is up and holds a real port; now ask for that exact one.
    let mut second = Command::new(bin())
        .args(["proxy", "--port", &port.to_string()])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_UPSTREAM", &upstream)
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // It must come up - on that port - within a few seconds.
    let mut took_over = false;
    for _ in 0..60 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(Some(st)) = second.try_wait() {
            panic!("the second proxy exited ({st}) instead of taking the port");
        }
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
            && matches!(first.try_wait(), Ok(Some(_)))
        {
            took_over = true;
            break;
        }
    }
    second.kill().ok();
    first.kill().ok();
    assert!(
        took_over,
        "the second proxy should hold the port and the first should be gone"
    );
}

/// A spent fleet actually WAITS, rather than the setting merely parsing.
///
/// `hold_seconds` is only worth anything if the turn is really held, so this
/// drives a proxy against an upstream that answers 429 forever and measures the
/// wall clock. The reset is seconds away and the ceiling is generous, so the
/// proxy should sleep and try again rather than return at once.
#[test]
fn hold_seconds_actually_delays_a_spent_turn() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    std::fs::create_dir_all(root.join(".local/share/swapdex")).unwrap();
    seed_slot(root, "acct", "uuid-a", "AT", true);

    let d = root.join(".local/share/swapdex");
    std::fs::write(
        d.join("settings.json"),
        // A ceiling of 5s: enough to prove the wait happens, short enough that
        // a wrong reset cannot hold the suite for a quarter of an hour.
        serde_json::to_vec(&serde_json::json!({"hold_seconds": 5})).unwrap(),
    )
    .unwrap();
    // An upstream that is always out of quota.
    let upstream = ControlledUpstream::start(|request| {
        let body = br#"{"type":"error","error":{"type":"rate_limit_error"}}"#;
        let _ =
            request.respond(tiny_http::Response::from_data(body.to_vec()).with_status_code(429));
    });
    let (child, pport) = start_proxy(root, upstream.url(), &[]);
    let child = ReapedChild::new(child);

    // Set the deadline only after the proxy is listening. Startup time used to
    // consume most of a reset fixed at setup, so a loaded runner could leave
    // only milliseconds to wait and fail the elapsed-time assertion without a
    // product error.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let reset = now + 3;
    let reset_deadline = std::time::UNIX_EPOCH + std::time::Duration::from_secs(reset as u64);
    std::fs::write(
        d.join("quota-cache.json"),
        serde_json::to_vec(&serde_json::json!({"acct": {
            "five_h": 100.0, "five_h_reset": reset,
            "seven_d": 100.0, "seven_d_reset": now + 900, "at": now}}))
        .unwrap(),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let _ = post_through(pport, r#"{"model":"claude-sonnet-5","messages":[]}"#);
    let waited = started.elapsed();
    let returned_at = std::time::SystemTime::now();
    child.stop();
    upstream.close();

    assert!(
        returned_at >= reset_deadline,
        "the turn came back in {waited:?}, before reset deadline {reset}"
    );
    assert!(
        waited < std::time::Duration::from_secs(60),
        "the turn was held {waited:?} - far past the reset it was waiting for"
    );
}

const CODEX_JWT_LAPSED: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjEwMDAwMDAwMDB9.sig";
const CODEX_JWT_LIVE: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.sig";
const CODEX_JWT_LIVE_NEW: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxNDI0NDQ4MDB9.new";
const CODEX_JWT_LIVE_B: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxNDI0NDQ4MDB9.account-b";
const CODEX_JWT_LIVE_B_NEW: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxNDI0NDQ4MDB9.account-b-renewed";

fn codex_id_token(subject: &str) -> String {
    use base64::Engine;
    let payload = serde_json::to_vec(&serde_json::json!({ "sub": subject })).unwrap();
    format!(
        "eyJhbGciOiJub25lIn0.{}.sig",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    )
}

/// A fake curl standing in for the OAuth token endpoint.
fn fake_oauth_curl(root: &std::path::Path, answer: &str, status: u16) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = root.join("fake-oauth-curl");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\nconfig=$(cat)\n\
             case \"$config\" in\n\
               *'/oauth/token'*) if [ -n \"$FAKE_OAUTH_COUNT\" ]; then printf x >> \"$FAKE_OAUTH_COUNT\"; fi ;;\n\
             esac\n\
             printf '{answer}\\n{status}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn start_codex_proxy_env(
    root: &std::path::Path,
    upstream: &str,
    envs: &[(&str, &str)],
) -> (std::process::Child, u16) {
    let mut child = Command::new(bin())
        .args(["proxy", "--port", "0", "--tool", "codex"])
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_UPSTREAM_CODEX", upstream)
        .env("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .env("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token")
        .envs(envs.iter().copied())
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

#[test]
fn claude_401_renews_the_same_account_before_returning_success() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "work", "id-work", "AT-OLD", true);
    let curl = fake_oauth_curl(
        root.path(),
        r#"{"access_token":"AT-NEW","refresh_token":"R2","expires_in":3600}"#,
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth.clone());
        let status = if auth == "Bearer AT-NEW" { 200 } else { 401 };
        request
            .respond(
                tiny_http::Response::from_string("{}")
                    .with_status_code(tiny_http::StatusCode(status)),
            )
            .unwrap();
    });
    let (proxy, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &["--auto", "--account", "work"],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_through_status(port, "{}"), 200);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["Bearer AT-OLD".to_string(), "Bearer AT-NEW".to_string()],
        "the refused turn is retried on the same selected account"
    );
    assert_eq!(std::fs::read(&count).unwrap(), b"x", "one OAuth exchange");
    proxy.stop();
    upstream.close();
}

#[test]
fn claude_repeated_401_is_bounded_to_one_same_account_recovery() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "work", "id-work", "AT-OLD", true);
    let curl = fake_oauth_curl(
        root.path(),
        r#"{"access_token":"AT-NEW","refresh_token":"R2","expires_in":3600}"#,
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth);
        request
            .respond(tiny_http::Response::from_string("{}").with_status_code(401))
            .unwrap();
    });
    let (proxy, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &[],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_through_status(port, "{}"), 401);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["Bearer AT-OLD".to_string(), "Bearer AT-NEW".to_string()],
        "the replacement's 401 is returned without a recovery loop"
    );
    assert_eq!(std::fs::read(&count).unwrap(), b"x", "one OAuth exchange");
    proxy.stop();
    upstream.close();
}

#[test]
fn claude_401_does_not_refresh_after_the_selected_identity_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "account-a", "id-work", "AT-OLD", true);
    let slot = root.path().join(".local/share/swapdex/slots/id-work");
    let identity_path = slot.join(".claude.json");
    let identity_a = br#"{"oauthAccount":{"accountUuid":"account-a","organizationUuid":"org-a"}}"#;
    let identity_b = br#"{"oauthAccount":{"accountUuid":"account-b","organizationUuid":"org-b"}}"#;
    std::fs::write(&identity_path, identity_a).unwrap();
    let original_credential = std::fs::read(slot.join(".credentials.json")).unwrap();
    let curl = fake_oauth_curl(
        root.path(),
        r#"{"access_token":"AT-NEW","refresh_token":"RT-NEW","expires_in":3600}"#,
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let replace_path = identity_path.clone();
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth.clone());
        let status = if auth == "Bearer AT-OLD" {
            std::fs::write(&replace_path, identity_b).unwrap();
            401
        } else {
            200
        };
        request
            .respond(
                tiny_http::Response::from_string("{}")
                    .with_status_code(tiny_http::StatusCode(status)),
            )
            .unwrap();
    });
    let (proxy, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &["--auto", "--account", "account-a"],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    let status = post_through_status(port, "{}");
    let output = proxy.stop_with_stdout();
    upstream.close();

    assert_eq!(status, 401, "replacement account B must not carry A's turn");
    assert_eq!(*seen.lock().unwrap(), vec!["Bearer AT-OLD".to_string()]);
    assert!(
        !count.exists(),
        "account B's selected chain must not be spent"
    );
    assert_eq!(
        std::fs::read(slot.join(".credentials.json")).unwrap(),
        original_credential
    );
    assert_eq!(std::fs::read(&identity_path).unwrap(), identity_b);
    assert!(
        !output.contains("account-a: renewed its login after upstream rejected it"),
        "account A must not be credited with an exchange after B replaced it: {output}"
    );
}

#[test]
fn codex_401_renews_the_same_account_before_returning_success() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "id-work",
        CODEX_JWT_LIVE,
        "acct-work",
        true,
    );
    let curl = fake_oauth_curl(
        root.path(),
        &format!(r#"{{"access_token":"{CODEX_JWT_LIVE_NEW}","refresh_token":"RT2"}}"#),
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let header = |name: &'static str| {
            request
                .headers()
                .iter()
                .find(|candidate| candidate.field.equiv(name))
                .map(|candidate| candidate.value.as_str().to_string())
                .unwrap_or_default()
        };
        let auth = header("authorization");
        sink.lock()
            .unwrap()
            .push((auth.clone(), header("chatgpt-account-id")));
        let status = if auth == format!("Bearer {CODEX_JWT_LIVE_NEW}") {
            200
        } else {
            401
        };
        request
            .respond(
                tiny_http::Response::from_string("{}")
                    .with_status_code(tiny_http::StatusCode(status)),
            )
            .unwrap();
    });
    let (proxy, port) = start_codex_proxy_env(
        root.path(),
        upstream.url(),
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_codex_turn(port).0, 200);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (format!("Bearer {CODEX_JWT_LIVE}"), "acct-work".into()),
            (format!("Bearer {CODEX_JWT_LIVE_NEW}"), "acct-work".into()),
        ],
        "the bearer changes while the selected account-id stays coherent"
    );
    assert_eq!(std::fs::read(&count).unwrap(), b"x", "one OAuth exchange");
    proxy.stop();
    upstream.close();
}

#[test]
fn codex_repeated_401_is_bounded_to_one_same_account_recovery() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "id-work",
        CODEX_JWT_LIVE,
        "acct-work",
        true,
    );
    let curl = fake_oauth_curl(
        root.path(),
        &format!(r#"{{"access_token":"{CODEX_JWT_LIVE_NEW}","refresh_token":"RT2"}}"#),
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth);
        request
            .respond(tiny_http::Response::from_string("{}").with_status_code(401))
            .unwrap();
    });
    let (proxy, port) = start_codex_proxy_env(
        root.path(),
        upstream.url(),
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_codex_turn(port).0, 401);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            format!("Bearer {CODEX_JWT_LIVE}"),
            format!("Bearer {CODEX_JWT_LIVE_NEW}"),
        ],
        "the replacement's 401 is returned without a recovery loop"
    );
    assert_eq!(std::fs::read(&count).unwrap(), b"x", "one OAuth exchange");
    proxy.stop();
    upstream.close();
}

#[test]
fn codex_401_does_not_refresh_a_replacement_subject_in_the_same_workspace() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "account-a",
        "id-work",
        CODEX_JWT_LIVE,
        "workspace-shared",
        true,
    );
    let auth_path = root
        .path()
        .join(".local/share/swapdex/slots/id-work/auth.json");
    std::fs::write(
        &auth_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": CODEX_JWT_LIVE,
                "refresh_token": "RT-A",
                "id_token": codex_id_token("subject-a"),
                "account_id": "workspace-shared"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let replacement = serde_json::to_vec_pretty(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": CODEX_JWT_LIVE_B,
            // Keep the fingerprint inputs equal to account A. The optional
            // ID-token subject must be the field that rejects this replacement.
            "refresh_token": "RT-A",
            "id_token": codex_id_token("subject-b"),
            "account_id": "workspace-shared"
        }
    }))
    .unwrap();
    let curl = fake_oauth_curl(
        root.path(),
        &format!(r#"{{"access_token":"{CODEX_JWT_LIVE_B_NEW}","refresh_token":"RT-B2"}}"#),
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let replace_path = auth_path.clone();
    let replacement_for_upstream = replacement.clone();
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth.clone());
        let status = if auth == format!("Bearer {CODEX_JWT_LIVE}") {
            std::fs::write(&replace_path, &replacement_for_upstream).unwrap();
            401
        } else {
            200
        };
        request
            .respond(
                tiny_http::Response::from_string("{}")
                    .with_status_code(tiny_http::StatusCode(status)),
            )
            .unwrap();
    });
    let (proxy, port) = start_codex_proxy_env(
        root.path(),
        upstream.url(),
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    let status = post_codex_turn(port).0;
    let output = proxy.stop_with_stdout();
    upstream.close();

    assert_eq!(status, 401, "replacement account B must not carry A's turn");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![format!("Bearer {CODEX_JWT_LIVE}")]
    );
    assert!(
        !count.exists(),
        "account B's refresh token must not be spent"
    );
    assert_eq!(std::fs::read(&auth_path).unwrap(), replacement);
    assert!(
        !output.contains("account-a: renewed its login after upstream rejected it"),
        "account A must not be credited with renewing account B: {output}"
    );
}

#[test]
fn codex_401_retries_a_same_account_replacement_without_oauth() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "id-work",
        CODEX_JWT_LIVE,
        "workspace-a",
        true,
    );
    let auth_path = root
        .path()
        .join(".local/share/swapdex/slots/id-work/auth.json");
    let auth = |access_token: &str| {
        serde_json::to_vec_pretty(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access_token,
                "refresh_token": "RT-A",
                "id_token": codex_id_token("subject-a"),
                "account_id": "workspace-a"
            }
        }))
        .unwrap()
    };
    std::fs::write(&auth_path, auth(CODEX_JWT_LIVE)).unwrap();
    let replacement = auth(CODEX_JWT_LIVE_B);
    let curl = fake_oauth_curl(
        root.path(),
        &format!(r#"{{"access_token":"{CODEX_JWT_LIVE_B_NEW}","refresh_token":"RT-A2"}}"#),
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let replace_path = auth_path.clone();
    let replacement_for_upstream = replacement.clone();
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth.clone());
        let status = if auth == format!("Bearer {CODEX_JWT_LIVE}") {
            std::fs::write(&replace_path, &replacement_for_upstream).unwrap();
            401
        } else if auth == format!("Bearer {CODEX_JWT_LIVE_B}") {
            200
        } else {
            401
        };
        request
            .respond(
                tiny_http::Response::from_string("{}")
                    .with_status_code(tiny_http::StatusCode(status)),
            )
            .unwrap();
    });
    let (proxy, port) = start_codex_proxy_env(
        root.path(),
        upstream.url(),
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_codex_turn(port).0, 200);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            format!("Bearer {CODEX_JWT_LIVE}"),
            format!("Bearer {CODEX_JWT_LIVE_B}"),
        ]
    );
    assert!(!count.exists(), "a replacement bearer needs no OAuth call");
    assert_eq!(std::fs::read(&auth_path).unwrap(), replacement);
    proxy.stop();
    upstream.close();
}

#[cfg(target_os = "linux")]
#[test]
fn an_unchanged_native_bearer_401_never_spends_its_refresh_token() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "work", "id-work", "SLOT-ACCESS", true);
    let slot = root.path().join(".local/share/swapdex/slots/id-work");
    let native = root.path().join(".claude");
    std::fs::create_dir_all(&native).unwrap();
    let identity = br#"{"oauthAccount":{"accountUuid":"same-user","organizationUuid":"same-org"}}"#;
    std::fs::write(slot.join(".claude.json"), identity).unwrap();
    std::fs::write(root.path().join(".claude.json"), identity).unwrap();
    std::fs::write(
        native.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"NATIVE-SAME","refreshToken":"NATIVE-RT","expiresAt":9999999999999}}"#,
    )
    .unwrap();
    let _native = spawn_native_cli(root.path(), "claude", &[("HOME", root.path())]);
    let curl = fake_oauth_curl(
        root.path(),
        r#"{"access_token":"MUST-NOT-BE-USED","expires_in":3600}"#,
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth);
        request
            .respond(tiny_http::Response::from_string("{}").with_status_code(401))
            .unwrap();
    });
    let (proxy, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &[],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_through_status(port, "{}"), 401);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["Bearer NATIVE-SAME".to_string()]
    );
    assert!(
        !count.exists(),
        "native ownership must spend zero OAuth calls"
    );
    proxy.stop();
    upstream.close();
}

#[cfg(target_os = "linux")]
#[test]
fn a_native_replacement_after_401_retries_without_oauth() {
    let root = tempfile::tempdir().unwrap();
    seed_slot(root.path(), "work", "id-work", "SLOT-ACCESS", true);
    let slot = root.path().join(".local/share/swapdex/slots/id-work");
    let native = root.path().join(".claude");
    std::fs::create_dir_all(&native).unwrap();
    let identity = br#"{"oauthAccount":{"accountUuid":"same-user","organizationUuid":"same-org"}}"#;
    std::fs::write(slot.join(".claude.json"), identity).unwrap();
    std::fs::write(root.path().join(".claude.json"), identity).unwrap();
    std::fs::write(
        native.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"NATIVE-OLD","refreshToken":"NATIVE-RT","expiresAt":9999999999999}}"#,
    )
    .unwrap();
    let _native = spawn_native_cli(root.path(), "claude", &[("HOME", root.path())]);
    let curl = fake_oauth_curl(
        root.path(),
        r#"{"access_token":"MUST-NOT-BE-USED","expires_in":3600}"#,
        200,
    );
    let count = root.path().join("oauth-count");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let replacement = native.join(".credentials.json");
    let upstream = ControlledUpstream::start(move |request| {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default();
        sink.lock().unwrap().push(auth.clone());
        let status = if auth == "Bearer NATIVE-OLD" {
            std::fs::write(
                &replacement,
                br#"{"claudeAiOauth":{"accessToken":"NATIVE-NEW","refreshToken":"NATIVE-RT2","expiresAt":9999999999999}}"#,
            )
            .unwrap();
            401
        } else {
            200
        };
        request
            .respond(
                tiny_http::Response::from_string("{}")
                    .with_status_code(tiny_http::StatusCode(status)),
            )
            .unwrap();
    });
    let (proxy, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &[],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
            ("FAKE_OAUTH_COUNT", count.to_str().unwrap()),
        ],
    );
    let proxy = ReapedChild::new(proxy);

    assert_eq!(post_through_status(port, "{}"), 200);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "Bearer NATIVE-OLD".to_string(),
            "Bearer NATIVE-NEW".to_string()
        ]
    );
    assert!(
        !count.exists(),
        "native replacement must spend zero OAuth calls"
    );
    proxy.stop();
    upstream.close();
}

/// The serving path asked only "is a login there".
///
/// swapdex already learned this once: `has_usable_login` says in as many words
/// that "asking only 'is a login there' sent turns to a slot whose token had
/// expired days earlier and reported the 401 that came back as a rejected
/// account". That lesson reached the ROTATION candidates and not the account
/// actually serving, so a lapsed Codex slot kept putting its dead bearer on
/// every turn - which is exactly what a machine here did for days.
#[test]
fn a_lapsed_codex_slot_renews_itself_before_serving_a_turn() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "id-work",
        CODEX_JWT_LAPSED,
        "acct-work",
        true,
    );
    let curl = fake_oauth_curl(
        root.path(),
        &format!(r#"{{"access_token":"{CODEX_JWT_LIVE}","refresh_token":"RT2"}}"#),
        200,
    );
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream(sink.clone());
    let (mut proxy, port) = start_codex_proxy_env(
        root.path(),
        &upstream,
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
        ],
    );
    let (status, _) = post_codex_turn(port);
    proxy.kill().ok();
    proxy.wait().ok();
    assert_eq!(status, 200);

    let seen = sink.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one turn reached the upstream: {seen:?}");
    assert_eq!(
        seen[0].0,
        format!("Bearer {CODEX_JWT_LIVE}"),
        "the renewed token served the turn, not the dead one: {seen:?}"
    );
}

/// When renewal cannot help, the honest answer is to get out of the way - the
/// same thing the Claude path does one branch above, and for the same reason:
/// serving a turn with a token known to be dead earns a 401 and names this
/// account as having paid for it.
#[test]
fn a_codex_slot_that_cannot_be_renewed_does_not_send_another_login() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot(
        root.path(),
        "work",
        "id-work",
        CODEX_JWT_LAPSED,
        "acct-work",
        true,
    );
    // The refresh token is spent too: only a sign-in fixes this.
    let curl = fake_oauth_curl(root.path(), r#"{"error":"invalid_grant"}"#, 400);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream(sink.clone());
    let (mut proxy, port) = start_codex_proxy_env(
        root.path(),
        &upstream,
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_CODEX_OAUTH_URL", "http://127.0.0.1:1/oauth/token"),
        ],
    );
    let (status, _) = post_codex_turn(port);
    proxy.kill().ok();
    proxy.wait().ok();
    assert_eq!(status, 502);

    let seen = sink.lock().unwrap().clone();
    assert!(
        seen.is_empty(),
        "no implicit client-account fallback: {seen:?}"
    );
}

/// Run a proxy that is expected to REFUSE, with a deadline.
///
/// The deadline is the point: if the refusal is missing the proxy binds a port
/// and serves forever, and a test that simply waited would hang rather than
/// report the defect.
fn proxy_refusal(root: &std::path::Path, args: &[&str]) -> String {
    let child = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(Ok(out)) => {
            assert!(
                !out.status.success(),
                "a proxy with nothing to serve must not report success"
            );
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        }
        Ok(Err(e)) => panic!("could not run the proxy: {e}"),
        Err(_) => {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            panic!("the proxy STARTED instead of refusing - it would forward your own login on every turn and never say so")
        }
    }
}

/// Register a Codex slot with no readable login at all.
fn seed_codex_slot_without_login(root: &std::path::Path, name: &str, id: &str) {
    seed_codex_slot(root, name, id, "T", "acct", true);
    std::fs::remove_file(
        root.join(".local/share/swapdex/slots")
            .join(id)
            .join("auth.json"),
    )
    .unwrap();
}

/// The startup refusal had no Codex half.
///
/// Its own note says a proxy that can read nothing "looks like it is working
/// while doing nothing it exists to do", and that the state "cost a full day".
/// The check reads Claude's credential, so it was skipped for Codex entirely -
/// a Codex proxy with nothing readable bound the port and forwarded the client's
/// own login on every turn, which is the very thing the refusal exists to stop.
#[test]
fn a_codex_proxy_with_nothing_readable_refuses_to_start() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot_without_login(root.path(), "work", "id-work");
    let out = proxy_refusal(root.path(), &["proxy", "--port", "0", "--tool", "codex"]);
    assert!(
        out.contains("nothing to serve turns with"),
        "and says why: {out}"
    );
}

/// One readable Codex login is enough - the refusal must not lock out a machine
/// that has a working account beside a signed-out one.
#[test]
fn a_codex_proxy_starts_when_one_account_is_readable() {
    let root = tempfile::tempdir().unwrap();
    seed_codex_slot_without_login(root.path(), "dead", "id-dead");
    seed_codex_slot(
        root.path(),
        "work",
        "id-work",
        CODEX_JWT_LIVE,
        "acct-work",
        true,
    );
    let sink = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake_codex_upstream(sink.clone());
    let (mut proxy, port) = start_codex_proxy(root.path(), &upstream, &[]);
    let (status, _) = post_codex_turn(port);
    proxy.kill().ok();
    proxy.wait().ok();
    assert_eq!(status, 200, "the readable account served the turn");
}

/// Managed requests use one selected credential; passthrough and native auth
/// exchanges preserve the client's authentication, including duplicate keys.
fn assert_api_key_boundary(tool: &str, passthrough: bool, auth_exchange: bool) {
    let root = tempfile::tempdir().unwrap();
    if tool == "codex" {
        seed_codex_slot(
            root.path(),
            "selected",
            "abc12345",
            "AT-SELECTED",
            "acct-selected",
            true,
        );
    } else {
        seed_slot(root.path(), "selected", "abc12345", "AT-SELECTED", true);
    }
    let paths = swapdex::paths::Paths::rooted(root.path());
    if passthrough {
        swapdex::slots::Slots::open_for(&paths, tool)
            .unwrap()
            .set_serving_off()
            .unwrap();
    }
    let preserve = passthrough || auth_exchange;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let upstream = ControlledUpstream::start(move |request| {
        let keys: Vec<_> = request
            .headers()
            .iter()
            .filter(|header| header.field.equiv("x-api-key"))
            .map(|header| header.value.to_string())
            .collect();
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("authorization"))
            .map(|header| header.value.to_string());
        let status = if !preserve && !keys.is_empty() {
            401
        } else {
            200
        };
        sink.lock().unwrap().push((keys, auth));
        request
            .respond(tiny_http::Response::from_string("{}").with_status_code(status))
            .unwrap();
    });
    let curl = fake_curl(root.path(), "unused");
    let (child, port) = start_proxy_with_env(
        root.path(),
        upstream.url(),
        &["--tool", tool],
        &[
            ("SWAPDEX_CURL", curl.to_str().unwrap()),
            ("SWAPDEX_UPSTREAM_CODEX", upstream.url()),
        ],
    );
    let proxy = ReapedChild::new(child);
    let path = if auth_exchange {
        "/v1/oauth/token"
    } else if tool == "codex" {
        "/v1/responses"
    } else {
        "/v1/messages"
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut statuses = Vec::new();
    for spelling in ["x-api-key", "X-API-Key"] {
        let mut response = agent
            .post(format!("http://127.0.0.1:{port}{path}"))
            .header("authorization", "Bearer CLIENT-TOKEN")
            .header(spelling, "CLIENT-KEY-ONE")
            .header(spelling, "CLIENT-KEY-TWO")
            .header("content-type", "application/json")
            .send(b"{}".as_slice())
            .unwrap();
        statuses.push(response.status().as_u16());
        response.body_mut().read_to_string().unwrap();
    }
    proxy.stop();
    upstream.close();
    assert_eq!(
        statuses,
        vec![200, 200],
        "{tool}: foreign keys must not reject the managed account"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one provider request per client request");
    for (keys, auth) in seen.iter() {
        if preserve {
            assert_eq!(keys, &["CLIENT-KEY-ONE", "CLIENT-KEY-TWO"]);
            assert_eq!(auth.as_deref(), Some("Bearer CLIENT-TOKEN"));
        } else {
            assert!(keys.is_empty(), "client API keys survived managed routing");
            assert_eq!(auth.as_deref(), Some("Bearer AT-SELECTED"));
        }
    }
}

#[test]
fn managed_claude_removes_client_api_key_headers() {
    assert_api_key_boundary("claude-code", false, false);
}

#[test]
fn managed_codex_removes_client_api_key_headers() {
    assert_api_key_boundary("codex", false, false);
}

#[test]
fn claude_passthrough_preserves_client_api_key_headers() {
    assert_api_key_boundary("claude-code", true, false);
}

#[test]
fn codex_passthrough_preserves_client_api_key_headers() {
    assert_api_key_boundary("codex", true, false);
}

#[test]
fn claude_auth_exchange_preserves_client_api_key_headers() {
    assert_api_key_boundary("claude-code", false, true);
}

#[test]
fn codex_auth_exchange_preserves_client_api_key_headers() {
    assert_api_key_boundary("codex", false, true);
}

mod streaming_delivery {
    use super::*;
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const PING: &[u8] = b"event: ping\ndata: {}\n\n";
    const STOP: &[u8] = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    fn start_stream_proxy(
        root: &std::path::Path,
        upstream: &str,
        tool: &str,
    ) -> (std::process::Child, u16) {
        start_proxy_with_env(
            root,
            upstream,
            &["--tool", tool],
            &[
                ("SWAPDEX_CURL", "/bin/false"),
                ("SWAPDEX_UPSTREAM_CODEX", upstream),
            ],
        )
    }

    fn received_body(wire: &[u8]) -> Vec<u8> {
        let Some(start) = wire.windows(4).position(|part| part == b"\r\n\r\n") else {
            return Vec::new();
        };
        let mut chunks = &wire[start + 4..];
        let mut body = Vec::new();
        while let Some(end) = chunks.windows(2).position(|part| part == b"\r\n") {
            let Ok(size) = usize::from_str_radix(std::str::from_utf8(&chunks[..end]).unwrap(), 16)
            else {
                break;
            };
            chunks = &chunks[end + 2..];
            if size == 0 || chunks.len() < size + 2 {
                break;
            }
            body.extend_from_slice(&chunks[..size]);
            assert_eq!(&chunks[size..size + 2], b"\r\n");
            chunks = &chunks[size + 2..];
        }
        body
    }

    fn receive_until(
        client: &mut TcpStream,
        wire: &mut Vec<u8>,
        ready: impl Fn(&[u8]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !ready(wire) {
            if Instant::now() >= deadline {
                return false;
            }
            let mut buffer = [0; 4096];
            match client.read(&mut buffer) {
                Ok(0) => return false,
                Ok(count) => wire.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("stream read failed: {error}"),
            }
        }
        true
    }

    fn assert_unbuffered_sse(tool: &str) {
        let root = tempfile::tempdir().unwrap();
        let (advance, next) = mpsc::channel();
        let (announced, sent) = mpsc::channel();
        let upstream = ControlledUpstream::start(move |mut request| {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let mut writer = request.into_writer();
            writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: Text/Event-Stream; charset=utf-8\r\nX-Relay-Test: preserved\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
            writer.flush().unwrap();
            announced.send(()).unwrap();
            for event in [PING, STOP] {
                next.recv_timeout(Duration::from_secs(5)).unwrap();
                write!(writer, "{:x}\r\n", event.len()).unwrap();
                writer.write_all(event).unwrap();
                writer.write_all(b"\r\n").unwrap();
                writer.flush().unwrap();
                announced.send(()).unwrap();
            }
            next.recv_timeout(Duration::from_secs(5)).unwrap();
            writer.write_all(b"0\r\n\r\n").unwrap();
            writer.flush().unwrap();
        });
        let (child, port) = if tool == "codex" {
            seed_codex_slot(root.path(), "a", "slot-a", "AT-A", "ACCT-A", true);
            start_stream_proxy(root.path(), upstream.url(), "codex")
        } else {
            seed_slot(root.path(), "a", "slot-a", "AT-A", true);
            start_stream_proxy(root.path(), upstream.url(), "claude-code")
        };
        let proxy = ReapedChild::new(child);
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        client.write_all(b"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
        sent.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut wire = Vec::new();
        let headers_early = receive_until(&mut client, &mut wire, |wire| {
            wire.windows(4).any(|part| part == b"\r\n\r\n")
        });
        advance.send(()).unwrap();
        sent.recv_timeout(Duration::from_secs(3)).unwrap();
        let ping_early = receive_until(&mut client, &mut wire, |wire| received_body(wire) == PING);
        advance.send(()).unwrap();
        sent.recv_timeout(Duration::from_secs(3)).unwrap();
        let expected = [PING, STOP].concat();
        let stop_early = receive_until(&mut client, &mut wire, |wire| {
            received_body(wire) == expected
        });
        advance.send(()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.read_to_end(&mut wire).unwrap();
        proxy.stop();
        upstream.close();
        assert!(String::from_utf8_lossy(&wire).starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(String::from_utf8_lossy(&wire)
            .to_ascii_lowercase()
            .contains("x-relay-test: preserved\r\n"));
        assert_eq!(
            received_body(&wire),
            expected,
            "SSE bytes changed in transit"
        );
        assert!(wire.ends_with(b"0\r\n\r\n"));
        assert!(headers_early && ping_early && stop_early,
            "{tool} withheld SSE while upstream remained open: headers={headers_early}, ping={ping_early}, completion={stop_early}");
    }

    #[test]
    fn claude_sse_is_delivered_while_upstream_remains_open() {
        assert_unbuffered_sse("claude");
    }

    #[test]
    fn codex_sse_is_delivered_while_upstream_remains_open() {
        assert_unbuffered_sse("codex");
    }

    #[test]
    fn sse_and_json_responses_can_share_a_keep_alive_connection() {
        let root = tempfile::tempdir().unwrap();
        seed_slot(root.path(), "a", "slot-a", "AT-A", true);
        let mut count = 0;
        let upstream = ControlledUpstream::start(move |mut request| {
            request.as_reader().read_to_end(&mut Vec::new()).unwrap();
            count += 1;
            let (body, content_type) = if count == 1 {
                (PING.to_vec(), "text/event-stream")
            } else {
                (b"{\"ok\":true}".to_vec(), "application/json")
            };
            request
                .respond(tiny_http::Response::from_data(body).with_header(
                    tiny_http::Header::from_bytes("content-type", content_type).unwrap(),
                ))
                .unwrap();
        });
        let (child, port) = start_stream_proxy(root.path(), upstream.url(), "claude-code");
        let proxy = ReapedChild::new(child);
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        client
            .write_all(
                b"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
            )
            .unwrap();
        let mut first = Vec::new();
        let complete = receive_until(&mut client, &mut first, |wire| wire.ends_with(b"0\r\n\r\n"));
        client.write_all(b"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
        let mut second = Vec::new();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.read_to_end(&mut second).unwrap();
        proxy.stop();
        upstream.close();
        assert!(complete, "first SSE response did not finish");
        assert_eq!(received_body(&first), PING);
        assert_eq!(received_body(&second), b"{\"ok\":true}");
        assert!(String::from_utf8_lossy(&second).starts_with("HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn head_and_bodyless_sse_responses_never_forward_a_body() {
        for (method, status) in [("HEAD", 200), ("GET", 204), ("GET", 205), ("GET", 304)] {
            let root = tempfile::tempdir().unwrap();
            seed_slot(root.path(), "a", "slot-a", "AT-A", true);
            let upstream = ControlledUpstream::start(move |request| {
                request
                    .respond(
                        tiny_http::Response::empty(tiny_http::StatusCode(status)).with_header(
                            tiny_http::Header::from_bytes("content-type", "text/event-stream")
                                .unwrap(),
                        ),
                    )
                    .unwrap();
            });
            let (child, port) = start_stream_proxy(root.path(), upstream.url(), "claude-code");
            let proxy = ReapedChild::new(child);
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            write!(
                client,
                "{method} /v1/messages HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut wire = Vec::new();
            client.read_to_end(&mut wire).unwrap();
            proxy.stop();
            upstream.close();
            let text = String::from_utf8(wire).unwrap();
            assert!(text.starts_with(&format!("HTTP/1.1 {status} ")));
            assert!(
                text.split_once("\r\n\r\n").unwrap().1.is_empty(),
                "{method} {status} sent a body"
            );
        }
    }

    #[test]
    fn invalid_upstream_sse_does_not_report_a_complete_http_body() {
        assert_invalid_upstream_closes(false);
    }

    #[test]
    fn invalid_upstream_sse_closes_a_keep_alive_connection() {
        assert_invalid_upstream_closes(true);
    }

    fn assert_invalid_upstream_closes(keep_alive: bool) {
        let root = tempfile::tempdir().unwrap();
        seed_slot(root.path(), "a", "slot-a", "AT-A", true);
        let upstream = ControlledUpstream::start(move |mut request| {
            request.as_reader().read_to_end(&mut Vec::new()).unwrap();
            let mut writer = request.into_writer();
            writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            write!(writer, "{:x}\r\n", PING.len()).unwrap();
            writer.write_all(PING).unwrap();
            writer.write_all(b"\r\nnot-hex\r\n").unwrap();
            writer.flush().unwrap();
        });
        let (child, port) = start_stream_proxy(root.path(), upstream.url(), "claude-code");
        let proxy = ReapedChild::new(child);
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let connection = if keep_alive { "keep-alive" } else { "close" };
        write!(client, "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nConnection: {connection}\r\n\r\n{{}}").unwrap();
        let mut wire = Vec::new();
        let closed = client.read_to_end(&mut wire).is_ok();
        client.shutdown(std::net::Shutdown::Both).ok();
        proxy.stop();
        upstream.close();
        assert!(closed, "failed SSE left the downstream connection open");
        assert!(String::from_utf8_lossy(&wire).starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(
            !wire.ends_with(b"0\r\n\r\n"),
            "upstream failure was turned into successful EOF"
        );
    }

    #[test]
    fn upstream_connection_headers_are_removed_from_sse() {
        let root = tempfile::tempdir().unwrap();
        seed_slot(root.path(), "a", "slot-a", "AT-A", true);
        let upstream = ControlledUpstream::start(move |mut request| {
            request.as_reader().read_to_end(&mut Vec::new()).unwrap();
            let mut writer = request.into_writer();
            writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive, X-Private-Hop\r\nX-Private-Hop: hidden\r\nKeep-Alive: timeout=100\r\nTE: trailers\r\nTrailer: X-Trailer\r\nUpgrade: h2c\r\nProxy-Authenticate: Basic realm=upstream\r\nX-End-To-End: preserved\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").unwrap();
            writer.flush().unwrap();
        });
        let (child, port) = start_stream_proxy(root.path(), upstream.url(), "claude-code");
        let proxy = ReapedChild::new(child);
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.write_all(b"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
        let mut wire = String::new();
        client.read_to_string(&mut wire).unwrap();
        proxy.stop();
        upstream.close();
        let headers = wire.split_once("\r\n\r\n").unwrap().0.to_ascii_lowercase();
        assert!(headers.contains("x-end-to-end: preserved\r\n"));
        for name in [
            "x-private-hop",
            "keep-alive",
            "te",
            "trailer",
            "upgrade",
            "proxy-authenticate",
        ] {
            assert!(
                !headers.contains(&format!("\r\n{name}:")),
                "forwarded connection header {name}"
            );
        }
    }
}
