#!/usr/bin/env python3
"""Opt-in stock-Codex compatibility check; fake credentials and loopback only.

Usage: python3 scripts/verify-codex-session-resume.py --codex /path/to/real/codex \
    --swapdex /path/to/built/swapdex
Requires Python 3 and a stock Codex supporting openai_base_url/app-server.
Every spawned process is terminated and reaped, including assertion failures.
"""

if not __debug__:
    raise SystemExit("verification requires assertions; rerun Python without -O")

import argparse
import base64
import contextlib
import datetime
import http.client
import http.server
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time


def stop(process):
    group = process.pid
    try:
        os.killpg(group, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        process.poll()
        try:
            os.killpg(group, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    else:
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def run(argv, env, timeout=45):
    process = subprocess.Popen(argv, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        assert process.returncode == 0, stderr.decode(errors="replace")[-2000:]
        return stdout
    finally:
        stop(process)


class TrackingHTTPServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, handler):
        super().__init__(address, handler)
        self.seen = []
        self.seen_lock = threading.Lock()
        self.target_port = None

    def record(self, request):
        with self.seen_lock:
            self.seen.append(request)

    def snapshot(self):
        with self.seen_lock:
            return list(self.seen)


def start_http_server(handler):
    server = TrackingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


def stop_http_server(server, thread):
    server.shutdown()
    server.server_close()
    thread.join(timeout=3)
    assert not thread.is_alive(), "HTTP fixture did not stop"


class QuietHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(10)

    def answer(self, body, content_type="application/json"):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Responses(QuietHandler):
    def record(self):
        self.server.record((self.command, self.path, self.headers.get("Upgrade"),
                            self.headers.get("Authorization"),
                            self.headers.get("chatgpt-account-id")))

    def do_GET(self):
        self.record()
        self.answer(b'{"models":[]}')

    def do_POST(self):
        self.record()
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        item = {"id": "msg_fixture", "type": "message", "role": "assistant",
                "status": "completed", "content": [
                    {"type": "output_text", "text": "fixture-ok", "annotations": []}]}
        response = {"id": "resp_fixture", "object": "response", "status": "completed",
                    "output": [item], "usage": {"input_tokens": 2, "output_tokens": 2,
                    "total_tokens": 4, "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens_details": {"reasoning_tokens": 0}}}
        events = [
            ("response.created", {"response": {**response, "status": "in_progress", "output": []}}),
            ("response.output_item.added", {"output_index": 0, "item": {**item, "status": "in_progress", "content": []}}),
            ("response.content_part.added", {"item_id": item["id"], "output_index": 0, "content_index": 0,
                                              "part": {"type": "output_text", "text": "", "annotations": []}}),
            ("response.output_text.delta", {"item_id": item["id"], "output_index": 0,
                                             "content_index": 0, "delta": "fixture-ok"}),
            ("response.output_text.done", {"item_id": item["id"], "output_index": 0,
                                            "content_index": 0, "text": "fixture-ok"}),
            ("response.output_item.done", {"output_index": 0, "item": item}),
            ("response.completed", {"response": response}),
        ]
        body = "".join(f"event: {kind}\ndata: {json.dumps({'type': kind, **value})}\n\n"
                       for kind, value in events).encode()
        self.answer(body, "text/event-stream")


class RejectDirect(QuietHandler):
    def reject(self):
        self.server.record((self.command, self.path))
        self.close_connection = True
        self.send_response(502)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = reject
    do_POST = reject


class ProxyRecorder(QuietHandler):
    def forward(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        headers = {name: value for name, value in self.headers.items()
                   if name.lower() not in {"host", "content-length"}}
        connection = http.client.HTTPConnection(
            "127.0.0.1", self.server.target_port, timeout=10)
        try:
            connection.request(self.command, self.path, body=body, headers=headers)
            response = connection.getresponse()
            response_body = response.read()
            status = response.status
            response_headers = response.getheaders()
        finally:
            connection.close()
        self.server.record((self.command, self.path, self.headers.get("Upgrade"), status))
        self.send_response(status)
        for name, value in response_headers:
            if name.lower() not in {"connection", "content-length", "transfer-encoding",
                                    "server", "date"}:
                self.send_header(name, value)
        self.send_header("Content-Length", str(len(response_body)))
        self.end_headers()
        if response_body:
            self.wfile.write(response_body)

    do_GET = forward
    do_POST = forward


class AppServer:
    def __init__(self, codex, env):
        self.process = subprocess.Popen([codex, "app-server", "--listen", "stdio://"],
            env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, start_new_session=True)
        self.buffer = b""
        self.counter = 0
        try:
            self.request("initialize", {"clientInfo": {"name": "swapdex-verification", "version": "1"},
                                         "capabilities": {"experimentalApi": True}})
            self.process.stdin.write(b'{"method":"initialized"}\n')
            self.process.stdin.flush()
        except BaseException:
            stop(self.process)
            raise

    def request(self, method, params):
        self.counter += 1
        self.process.stdin.write((json.dumps({"id": self.counter, "method": method, "params": params}) + "\n").encode())
        self.process.stdin.flush()
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                value = json.loads(line)
                if value.get("id") == self.counter:
                    return value
            if select.select([self.process.stdout], [], [], 0.2)[0]:
                chunk = os.read(self.process.stdout.fileno(), 65536)
                assert chunk, "app-server closed stdout"
                self.buffer += chunk
        raise TimeoutError(method)

    def close(self):
        if self.process is not None:
            process, self.process = self.process, None
            stop(process)

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()


def token():
    def part(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).rstrip(b"=").decode()
    return part({"alg": "none"}) + "." + part({"exp": 4102444800, "sub": "fixture-user",
        "email": "fixture@example.com", "https://api.openai.com/auth": {
            "chatgpt_account_id": "fixture-account", "chatgpt_plan_type": "plus"}}) + ".fixture"


def verify(codex, swapdex, root):
    home = root / ".codex"
    home.mkdir()
    workspace = root / "workspace"
    workspace.mkdir()
    temporary = root / "tmp"
    temporary.mkdir()
    env = {"PATH": os.environ.get("PATH", os.defpath)}
    for name in ("LANG", "LC_ALL", "LOGNAME", "SHELL", "TERM", "TZ", "USER"):
        if name in os.environ:
            env[name] = os.environ[name]
    env.update(HOME=str(root), CODEX_HOME=str(home), SWAPDEX_ROOT=str(root),
               XDG_CACHE_HOME=str(root / ".cache"), XDG_CONFIG_HOME=str(root / ".config"),
               XDG_DATA_HOME=str(root / ".local/share"), XDG_STATE_HOME=str(root / ".local/state"),
               TMPDIR=str(temporary), NO_COLOR="1",
               NO_PROXY="localhost,127.0.0.1", no_proxy="localhost,127.0.0.1")
    jwt = token()
    (home / "auth.json").write_text(json.dumps({"auth_mode": "chatgpt", "tokens": {
        "id_token": jwt, "access_token": jwt, "refresh_token": "fixture-refresh",
        "account_id": "fixture-account"}, "last_refresh":
        datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")}))

    with contextlib.ExitStack() as cleanup:
        upstream, upstream_thread = start_http_server(Responses)
        cleanup.callback(stop_http_server, upstream, upstream_thread)
        direct, direct_thread = start_http_server(RejectDirect)
        cleanup.callback(stop_http_server, direct, direct_thread)
        env["SWAPDEX_UPSTREAM_CODEX"] = f"http://127.0.0.1:{upstream.server_port}"
        run([swapdex, "serve", "--off", "--tool", "codex"], env)
        proxy = subprocess.Popen([swapdex, "proxy", "--port", "0", "--tool", "codex"],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        cleanup.callback(stop, proxy)
        marker = root / ".local/share/swapdex/proxy-codex"
        deadline = time.monotonic() + 10
        while not marker.exists():
            assert proxy.poll() is None and time.monotonic() < deadline, "proxy startup failed"
            time.sleep(0.05)
        port = int(marker.read_text().split()[1])
        recorder, recorder_thread = start_http_server(ProxyRecorder)
        recorder.target_port = port
        cleanup.callback(stop_http_server, recorder, recorder_thread)
        (home / "config.toml").write_text(
            f'model="gpt-6"\nopenai_base_url="http://127.0.0.1:{recorder.server_port}/v1"\n'
            f'chatgpt_base_url="http://127.0.0.1:{direct.server_port}"\n'
            f'[projects.{json.dumps(str(workspace))}]\ntrust_level="trusted"\n')
        output = run([codex, "-C", str(workspace), "exec", "--skip-git-repo-check", "--json",
                      "Return fixture-ok."], env)
        events = [json.loads(line) for line in output.splitlines() if line.startswith(b"{")]
        assert any(e.get("type") == "turn.completed" for e in events), events
        assert any(e.get("item", {}).get("text") == "fixture-ok" for e in events), events

        transport = recorder.snapshot()
        probes = [request for request in transport
                  if request[0] == "GET" and request[1].endswith("/responses")
                  and (request[2] or "").lower() == "websocket"]
        posts = [request for request in transport
                 if request[0] == "POST" and request[1].endswith("/responses")]
        assert len(probes) == 1 and probes[0][3] == 426, transport
        assert len(posts) == 1 and posts[0][3] == 200, transport
        model_requests = upstream.snapshot()
        response_requests = [request for request in model_requests
                             if request[0] == "POST" and request[1].endswith("/responses")]
        assert len(response_requests) == 1, model_requests
        assert all((method == "GET" and path.startswith("/models?"))
                   or (method == "POST" and path.endswith("/responses"))
                   for method, path, _upgrade, _authorization, _account in model_requests), model_requests
        assert all(upgrade is None and authorization == f"Bearer {jwt}"
                   and account == "fixture-account"
                   for _method, _path, upgrade, authorization, account in model_requests), model_requests
        direct_requests = direct.snapshot()
        assert not any(path.split("?", 1)[0].endswith(("/models", "/responses"))
                       for _method, path in direct_requests), direct_requests
        print("PASS stock Codex receives one 426 and completes one HTTP turn through Swapdex", flush=True)

        rollouts = list((home / "sessions").rglob("*.jsonl"))
        assert len(rollouts) == 1, rollouts
        rollout = rollouts[0]
        header, body = rollout.read_bytes().split(b"\n", 1)
        meta = json.loads(header)
        tid = meta["payload"]["id"]
        meta["payload"]["model_provider"] = "swapdex-fixture"
        meta["payload"]["source"] = "cli"
        header = json.dumps(meta, separators=(",", ":")).encode()
        legacy_bytes = header + b"\n" + body
        rollout.write_bytes(legacy_bytes)
        legacy_stat = rollout.stat()
        provider_token = json.dumps("swapdex-fixture").encode()
        provider_start = header.index(provider_token)
        provider_end = provider_start + len(provider_token)
        with sqlite3.connect(home / "state_5.sqlite") as db:
            changed = db.execute(
                "UPDATE threads SET model_provider='swapdex-fixture', source='cli' WHERE id=?",
                (tid,))
            assert changed.rowcount == 1
            columns = [column[1] for column in db.execute("PRAGMA table_info(threads)")]
            legacy_row = db.execute("SELECT * FROM threads WHERE id=?", (tid,)).fetchone()
        assert legacy_row is not None

        with AppServer(codex, env) as server:
            before = server.request("thread/list", {"modelProviders": ["openai"], "limit": 100})
            assert not any(t["id"] == tid for t in before["result"]["data"]), before
            missing = server.request("thread/resume", {"threadId": tid})
            assert "swapdex-fixture" in json.dumps(missing.get("error", {})), missing
        print("PASS legacy provider reproduces hidden picker and missing-provider resume", flush=True)

        run([swapdex, "repair-codex-sessions"], env)
        repaired_bytes = rollout.read_bytes()
        changed_header, _changed_body = repaired_bytes.split(b"\n", 1)
        assert rollout.stat().st_ino == legacy_stat.st_ino
        assert len(repaired_bytes) == len(legacy_bytes)
        assert repaired_bytes[:provider_start] == legacy_bytes[:provider_start]
        assert repaired_bytes[provider_end:] == legacy_bytes[provider_end:]
        assert json.loads(changed_header)["payload"]["model_provider"] == "openai"

        with sqlite3.connect(home / "state_5.sqlite") as db:
            repaired_row = db.execute("SELECT * FROM threads WHERE id=?", (tid,)).fetchone()
        provider_column = columns.index("model_provider")
        assert repaired_row[provider_column] == "openai", repaired_row
        assert all(before == after for index, (before, after) in enumerate(
            zip(legacy_row, repaired_row)) if index != provider_column), (legacy_row, repaired_row)

        with AppServer(codex, env) as server:
            after = server.request("thread/list", {"modelProviders": ["openai"], "limit": 100})
            assert any(t["id"] == tid for t in after["result"]["data"]), after
            resumed = server.request("thread/resume", {"threadId": tid})
            assert "error" not in resumed, resumed
            assert resumed["result"]["modelProvider"] == "openai"
            assert resumed["result"]["thread"]["id"] == tid
        final_transport = recorder.snapshot()
        assert sum(method == "GET" and path.endswith("/responses")
                   and (upgrade or "").lower() == "websocket" and status == 426
                   for method, path, upgrade, status in final_transport) == 1, final_transport
        assert sum(method == "POST" and path.endswith("/responses") and status == 200
                   for method, path, _upgrade, status in final_transport) == 1, final_transport
        final_upstream = upstream.snapshot()
        final_responses = sum(method == "POST" and path.endswith("/responses")
                              for method, path, _upgrade, _authorization, _account in final_upstream)
        assert final_responses == 1, final_upstream
        assert all(upgrade is None and authorization == f"Bearer {jwt}"
                   and account == "fixture-account"
                   for _method, _path, upgrade, authorization, account in final_upstream), final_upstream
        direct_requests = direct.snapshot()
        assert not any(path.split("?", 1)[0].endswith(("/models", "/responses"))
                       for _method, path in direct_requests), direct_requests
        print("PASS stock native listing and resume recover the same thread; conversation bytes preserved", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--swapdex", required=True)
    arguments = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="swapdex-native-resume-") as temporary:
        verify(str(Path(arguments.codex).resolve()), str(Path(arguments.swapdex).resolve()), Path(temporary))
