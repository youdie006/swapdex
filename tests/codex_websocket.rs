use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

struct RunningProxy(Option<Child>);

impl RunningProxy {
    fn start(root: &std::path::Path, tool: &str, upstream: &str) -> (Self, u16) {
        let off = Command::new(bin())
            .args(["serve", "--off", "--tool", tool])
            .env("SWAPDEX_ROOT", root)
            .output()
            .unwrap();
        assert!(
            off.status.success(),
            "could not enable passthrough: {off:?}"
        );

        let upstream_var = if tool == "codex" {
            "SWAPDEX_UPSTREAM_CODEX"
        } else {
            "SWAPDEX_UPSTREAM"
        };
        let mut child = Command::new(bin())
            .args(["proxy", "--port", "0", "--tool", tool])
            .env("SWAPDEX_ROOT", root)
            .env(upstream_var, upstream)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let announcement = read_line(child.stdout.as_mut().unwrap());
        let port = announcement
            .rsplit(':')
            .next()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("proxy did not announce a port: {announcement}"));
        (Self(Some(child)), port)
    }

    fn next_stdout_line(&mut self) -> String {
        read_line(self.0.as_mut().unwrap().stdout.as_mut().unwrap())
    }

    fn stop(mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().unwrap();
        }
    }
}

impl Drop for RunningProxy {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

fn read_line(reader: &mut impl Read) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while reader.read(&mut byte).unwrap_or(0) == 1 {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8(line).unwrap()
}

struct CountingUpstream {
    url: String,
    paths: Arc<Mutex<Vec<String>>>,
    server: Arc<tiny_http::Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CountingUpstream {
    fn start() -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let serving = Arc::clone(&server);
        let seen = Arc::clone(&paths);
        let thread = std::thread::spawn(move || {
            for request in serving.incoming_requests() {
                seen.lock().unwrap().push(request.url().to_string());
                request
                    .respond(
                        tiny_http::Response::from_string("{}")
                            .with_status_code(tiny_http::StatusCode(200)),
                    )
                    .ok();
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            paths,
            server,
            thread: Some(thread),
        }
    }

    fn close(mut self) -> Vec<String> {
        self.server.unblock();
        self.thread.take().unwrap().join().unwrap();
        self.paths.lock().unwrap().clone()
    }
}

impl Drop for CountingUpstream {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.server.unblock();
            thread.join().unwrap();
        }
    }
}

fn get(port: u16, path: &str, websocket: bool) -> Result<u16, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let upgrade = if websocket {
        "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: c3dhcGRleA==\r\n"
    } else {
        "Connection: close\r\n"
    };
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{upgrade}\r\n"
    )
    .unwrap();
    stream.flush().unwrap();

    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => response.push(byte[0]),
            Err(error) => return Err(error.to_string()),
        }
    }
    let response = String::from_utf8(response).map_err(|error| error.to_string())?;
    response
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("invalid HTTP response: {response:?}"))?
        .parse()
        .map_err(|error| format!("invalid HTTP status in {response:?}: {error}"))
}

#[test]
fn codex_responses_websocket_probes_fall_back_without_reaching_upstream() {
    let root = tempfile::tempdir().unwrap();
    let upstream = CountingUpstream::start();
    let (proxy, port) = RunningProxy::start(root.path(), "codex", &upstream.url);

    let versioned = get(port, "/v1/responses", true);
    let unversioned = get(port, "/responses", true);
    let ordinary = get(port, "/v1/responses", false);

    proxy.stop();
    let seen = upstream.close();
    assert_eq!(
        seen,
        ["/responses"],
        "WebSocket probes must not require an account or contact upstream"
    );
    assert_eq!(versioned, Ok(426));
    assert_eq!(unversioned, Ok(426));
    assert_eq!(ordinary, Ok(200), "ordinary Responses HTTP still forwards");
}

#[test]
fn manual_codex_hint_uses_the_builtin_openai_provider() {
    let root = tempfile::tempdir().unwrap();
    let upstream = CountingUpstream::start();
    let (mut proxy, port) = RunningProxy::start(root.path(), "codex", &upstream.url);

    let hint = proxy.next_stdout_line();

    proxy.stop();
    upstream.close();
    assert_eq!(
        hint,
        format!("  point Codex at it:  codex -c openai_base_url=http://127.0.0.1:{port}/v1")
    );
}
