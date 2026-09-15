#!/usr/bin/env python3
"""Smoke-test an installed Swapdex binary with fake clients and loopback APIs."""

import argparse
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time


SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def stop_process(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=3)
    if process.stdout:
        process.stdout.close()
    if process.stderr:
        process.stderr.close()


def run(argv, env, expected=0, timeout=20):
    process = subprocess.Popen(
        argv,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        stop_process(process)
        raise
    require(
        expected is None or process.returncode == expected,
        f"command {Path(argv[0]).name} {' '.join(argv[1:3])} exited {process.returncode}: "
        f"{stderr.decode(errors='replace')[-1000:]}",
    )
    return process.returncode, stdout, stderr


def safe_env(root, native_bin):
    temporary = root / "tmp"
    temporary.mkdir()
    return {
        "PATH": f"{native_bin}:{SYSTEM_PATH}",
        "HOME": str(root),
        "SWAPDEX_ROOT": str(root),
        "XDG_CACHE_HOME": str(root / ".cache"),
        "XDG_CONFIG_HOME": str(root / ".config"),
        "XDG_DATA_HOME": str(root / ".local/share"),
        "XDG_STATE_HOME": str(root / ".local/state"),
        "TMPDIR": str(temporary),
        "SHELL": "/bin/sh",
        "USER": "swapdex-fixture",
        "LOGNAME": "swapdex-fixture",
        "LANG": "C",
        "LC_ALL": "C",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "NO_PROXY": "localhost,127.0.0.1",
        "no_proxy": "localhost,127.0.0.1",
        "SWAPDEX_OAUTH_URL": "http://127.0.0.1:1/oauth/token",
        "SWAPDEX_CODEX_OAUTH_URL": "http://127.0.0.1:1/oauth/token",
        "SWAPDEX_FAKE_CLIENT_LOG": str(root / "native-client.log"),
    }


def write_fake_client(path, tool):
    home = "CLAUDE_CONFIG_DIR" if tool == "claude" else "CODEX_HOME"
    base = "ANTHROPIC_BASE_URL" if tool == "claude" else "CODEX_UNUSED_BASE"
    path.write_text(
        "#!/bin/sh\n"
        "{\n"
        f"  printf 'BEGIN\\ntool={tool}\\n'\n"
        f"  printf 'home=%s\\n' \"${{{home}-}}\"\n"
        f"  printf 'base=%s\\n' \"${{{base}-}}\"\n"
        "  for sx_arg in \"$@\"; do printf 'arg=%s\\n' \"$sx_arg\"; done\n"
        "  printf 'END\\n'\n"
        "} >> \"$SWAPDEX_FAKE_CLIENT_LOG\"\n",
        encoding="utf-8",
    )
    path.chmod(0o755)


def calls(root):
    path = root / "native-client.log"
    if not path.exists():
        return []
    records = []
    current = None
    for line in path.read_text(encoding="utf-8").splitlines():
        if line == "BEGIN":
            current = []
        elif line == "END":
            require(current is not None, "malformed fake-client record")
            records.append(current)
            current = None
        elif current is not None:
            current.append(line)
    require(current is None, "incomplete fake-client record")
    return records


def tool_args(tool):
    return ["--tool", tool]


def store_dir(root):
    return root / ".local/share/swapdex"


def short_tool(tool):
    return "claude" if tool == "claude" else "codex"


def slot_dir(root, name, tool):
    records = json.loads((store_dir(root) / "slots.json").read_text(encoding="utf-8"))
    stored_tool = "claude-code" if tool == "claude" else "codex"
    matches = [
        Path(record["config_dir"])
        for record in records
        if record["name"] == name and record.get("tool", "claude-code") == stored_tool
    ]
    require(len(matches) == 1, f"expected one {tool} slot named {name}")
    slot = matches[0].resolve()
    require(root.resolve() in slot.parents, "slot escaped the synthetic root")
    return slot


def seed_accounts(swapdex, root, env):
    for tool in ("claude", "codex"):
        for name in ("a", "b"):
            run([swapdex, "run", name, "--no-launch", *tool_args(tool)], env)
            slot = slot_dir(root, name, tool)
            if tool == "claude":
                (slot / ".credentials.json").write_text(
                    json.dumps({"claudeAiOauth": {
                        "accessToken": f"fixture-claude-{name}",
                        "refreshToken": "fixture-refresh",
                        "expiresAt": 4102444800000,
                    }}),
                    encoding="utf-8",
                )
                (slot / ".claude.json").write_text(
                    json.dumps({"oauthAccount": {
                        "accountUuid": f"fixture-uuid-{name}",
                        "emailAddress": f"fixture-{name}@example.com",
                    }}),
                    encoding="utf-8",
                )
            else:
                (slot / "auth.json").write_text(
                    json.dumps({"auth_mode": "chatgpt", "tokens": {
                        "access_token": f"fixture-codex-{name}",
                        "refresh_token": "fixture-refresh",
                        "account_id": f"fixture-account-{name}",
                    }}),
                    encoding="utf-8",
                )
        run([swapdex, "use", "a", *tool_args(tool)], env)
        run([swapdex, "serve", "a", *tool_args(tool)], env)


def verify_named_runs(swapdex, root, env):
    shim_dir = store_dir(root) / "bin"
    shim_env = dict(env, PATH=f"{shim_dir}:{env['PATH']}")
    for tool in ("claude", "codex"):
        short = short_tool(tool)
        active = store_dir(root) / f"active-{short}"
        serving = store_dir(root) / f"serving-{short}"
        before = active.read_bytes(), serving.read_bytes()
        count = len(calls(root))
        fresh = f"fresh-{short}"
        run([swapdex, "run", fresh, *tool_args(tool), "--", "fixture-run"], shim_env)
        after_calls = calls(root)
        require(len(after_calls) == count + 1, f"named {tool} run missed native client")
        record = after_calls[-1]
        homes = [line[5:] for line in record if line.startswith("home=")]
        require(len(homes) == 1 and homes[0], f"named {tool} run did not report its home")
        require(Path(homes[0]).resolve() == slot_dir(root, fresh, tool),
                f"named {tool} run used wrong home")
        require("arg=fixture-run" in record, f"named {tool} run lost arguments")
        require(not any("openai_base_url=" in line for line in record), f"named {tool} run used proxy")
        require((active.read_bytes(), serving.read_bytes()) == before, f"named {tool} run changed pointers")


def verify_shims(root, env):
    shim_dir = store_dir(root) / "bin"
    cases = {
        "claude": {
            "shim": shim_dir / "claude",
            "ordinary": ["chat"],
            "prompt": ["-p", "login"],
            "bypass": [["auth", "login"], ["--help"]],
        },
        "codex": {
            "shim": shim_dir / "codex",
            "ordinary": ["resume"],
            "prompt": ["exec", "login"],
            "bypass": [["login"], ["--help"]],
        },
    }
    for tool, case in cases.items():
        marker = store_dir(root) / f"serving-{short_tool(tool)}"
        marker.write_text("relative-invalid-state", encoding="utf-8")
        for argv in (case["ordinary"], case["prompt"]):
            count = len(calls(root))
            status, _stdout, stderr = run([str(case["shim"]), *argv], env, expected=None)
            require(status != 0, f"managed {tool} startup failure was ignored")
            require(len(calls(root)) == count, f"managed {tool} failure launched native client")
            require(b"managed proxy startup failed" in stderr, f"managed {tool} failure was unclear")
        for argv in case["bypass"]:
            count = len(calls(root))
            run([str(case["shim"]), *argv], env)
            require(len(calls(root)) == count + 1, f"{tool} auth/help bypass did not reach native client")


class RecorderServer(http.server.ThreadingHTTPServer):
    daemon_threads = False

    def __init__(self):
        super().__init__(("127.0.0.1", 0), RecorderHandler)
        self.records = []
        self.records_lock = threading.Lock()

    def record(self, value):
        with self.records_lock:
            self.records.append(value)

    def snapshot(self):
        with self.records_lock:
            return list(self.records)


class RecorderHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(5)

    def read_body(self):
        if self.headers.get("Transfer-Encoding", "").lower() != "chunked":
            return self.rfile.read(int(self.headers.get("Content-Length", "0")))
        body = bytearray()
        while True:
            size = int(self.rfile.readline().split(b";", 1)[0], 16)
            if size == 0:
                while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                    pass
                return bytes(body)
            body.extend(self.rfile.read(size))
            require(self.rfile.read(2) == b"\r\n", "invalid chunked fixture request")

    def do_POST(self):
        body = self.read_body()
        user_id = None
        try:
            user_id = json.loads(body).get("metadata", {}).get("user_id")
        except (UnicodeDecodeError, json.JSONDecodeError):
            pass
        self.server.record({
            "authorization": self.headers.get("Authorization"),
            "account": self.headers.get("chatgpt-account-id"),
            "path": self.path,
            "user_id": user_id,
        })
        response = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


def start_server():
    server = RecorderServer()
    thread = threading.Thread(target=server.serve_forever, name="swapdex-fixture-upstream")
    thread.start()
    return server, thread


def stop_server(server, thread):
    try:
        server.shutdown()
    finally:
        server.server_close()
        thread.join(timeout=3)
    require(not thread.is_alive(), "loopback fixture did not stop")


def marker_for(root, tool):
    return store_dir(root) / ("proxy" if tool == "claude" else "proxy-codex")


def start_proxy(swapdex, root, env, tool, upstream):
    marker = marker_for(root, tool)
    try:
        marker.unlink()
    except FileNotFoundError:
        pass
    proxy_env = dict(env)
    variable = "SWAPDEX_UPSTREAM" if tool == "claude" else "SWAPDEX_UPSTREAM_CODEX"
    proxy_env[variable] = f"http://127.0.0.1:{upstream.server_port}"
    process = subprocess.Popen(
        [swapdex, "proxy", "--port", "0", *tool_args(tool)],
        env=proxy_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if marker.exists():
                fields = marker.read_text(encoding="utf-8").split()
                if len(fields) >= 3 and fields[0].isdigit() and fields[1].isdigit():
                    require(int(fields[0]) == process.pid, f"{tool} proxy marker named another process")
                    return process, int(fields[1]), marker.read_bytes()
            require(process.poll() is None, f"{tool} proxy exited during startup")
            time.sleep(0.05)
        raise AssertionError(f"{tool} proxy did not become ready")
    except BaseException:
        stop_process(process)
        raise


def post_turn(connection, tool):
    if tool == "claude":
        path = "/v1/messages"
        body = json.dumps({"model": "fixture", "messages": [], "metadata": {
            "user_id": json.dumps({"account_uuid": "fixture-uuid-a"})
        }})
        headers = {"Authorization": "Bearer fixture-client", "Content-Type": "application/json"}
    else:
        path = "/v1/responses"
        body = '{"input":[]}'
        headers = {
            "Authorization": "Bearer fixture-client",
            "chatgpt-account-id": "fixture-client",
            "Content-Type": "application/json",
        }
    connection.request("POST", path, body=body, headers=headers)
    response = connection.getresponse()
    response_body = response.read()
    require(response.status == 200 and response_body == b'{"ok":true}', f"{tool} proxy turn failed")


def verify_live_switches(swapdex, root, env):
    for tool in ("claude", "codex"):
        run([swapdex, "serve", "a", *tool_args(tool)], env)
        active = store_dir(root) / f"active-{short_tool(tool)}"
        active_before = active.read_bytes()
        upstream, upstream_thread = start_server()
        proxy = None
        connection = None
        marker = marker_for(root, tool)
        try:
            proxy, port, marker_before = start_proxy(swapdex, root, env, tool, upstream)
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            connection.connect()
            socket_identity = connection.sock.getsockname()
            for name in ("a", "b", "a"):
                run([swapdex, "serve", name, *tool_args(tool)], env)
                require(proxy.poll() is None, f"{tool} proxy restarted before next turn")
                require(marker.read_bytes() == marker_before, f"{tool} proxy marker changed")
                post_turn(connection, tool)
                require(connection.sock is not None, f"{tool} proxy closed the client connection")
                require(connection.sock.getsockname() == socket_identity, f"{tool} client connection changed")
            require(active.read_bytes() == active_before, f"{tool} serving changes moved active pointer")
            seen = upstream.snapshot()
            require(len(seen) == 3, f"{tool} upstream saw the wrong turn count")
            expected_auth = [f"Bearer fixture-{tool}-{name}" for name in ("a", "b", "a")]
            require([item["authorization"] for item in seen] == expected_auth, f"{tool} payer order was not A-B-A")
            if tool == "claude":
                expected_ids = [f"fixture-uuid-{name}" for name in ("a", "b", "a")]
                require(all(expected in (item["user_id"] or "")
                            for item, expected in zip(seen, expected_ids)),
                        "Claude body identity did not follow the payer")
            else:
                expected_accounts = [f"fixture-account-{name}" for name in ("a", "b", "a")]
                require([item["account"] for item in seen] == expected_accounts,
                        "Codex account header did not follow the payer")
        finally:
            try:
                if connection is not None:
                    connection.close()
            finally:
                try:
                    if proxy is not None:
                        stop_process(proxy)
                finally:
                    stop_server(upstream, upstream_thread)
        require(not marker.exists(), f"{tool} proxy marker remained after shutdown")


def verify(swapdex, root):
    native_bin = root / "native-bin"
    native_bin.mkdir()
    write_fake_client(native_bin / "claude", "claude")
    write_fake_client(native_bin / "codex", "codex")
    env = safe_env(root, native_bin)
    seed_accounts(swapdex, root, env)
    run([swapdex, "shim"], env)
    require((store_dir(root) / "bin/claude").is_file(), "Claude shim was not installed")
    require((store_dir(root) / "bin/codex").is_file(), "Codex shim was not installed")
    verify_named_runs(swapdex, root, env)
    verify_shims(root, env)
    verify_live_switches(swapdex, root, env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--swapdex", required=True, help="absolute path to the installed executable")
    arguments = parser.parse_args()
    supplied = Path(arguments.swapdex)
    require(supplied.is_absolute(), "--swapdex must be an absolute path")
    swapdex = supplied.resolve()
    require(swapdex.is_file() and os.access(swapdex, os.X_OK), "--swapdex is not executable")
    with tempfile.TemporaryDirectory(prefix="swapdex-installed-routing-") as temporary:
        verify(str(swapdex), Path(temporary))
    print("PASS installed shims, direct named runs, and live Claude/Codex A-B-A routing", flush=True)


if __name__ == "__main__":
    main()
