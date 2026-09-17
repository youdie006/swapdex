#!/usr/bin/env python3
"""Exercise two coordinated renewals against a live stock Claude process.

Everything mutable lives under a temporary HOME. OAuth is a local fake curl,
model inference is a loopback SSE server, and external HTTPS uses a dead proxy.
The script never reads or writes a real Claude login.
"""

import argparse
import contextlib
import hashlib
import http.server
import json
import os
import selectors
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path
from urllib.parse import urlsplit


class KeychainUnavailable(Exception):
    """The isolated SSH fixture cannot access its exact Keychain item."""


class KeychainFixture:
    def __init__(self, slot, account, env):
        self.service = "Claude Code-credentials-" + hashlib.sha256(
            os.fsencode(slot)
        ).hexdigest()[:8]
        self.account = account
        self.env = env
        self.label = "swapdex-renewal-fixture-" + uuid.uuid4().hex
        self.created = False

    def command(self, *args, input=None):
        try:
            return subprocess.run(["/usr/bin/security", *args], input=input,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env,
                timeout=10, check=False)
        except subprocess.TimeoutExpired as error:
            raise KeychainUnavailable(
                f"security {args[0]} timed out under SSH"
            ) from error

    def assert_absent(self):
        # The service is random-slot-derived. Check without -a so a collision
        # with ANY account prevents creation, even if our account differs.
        result = self.command("find-generic-password", "-s", self.service)
        if result.returncode == 0:
            raise KeychainUnavailable("fixture Keychain service already exists; refusing overwrite")
        if result.returncode != 44:
            raise KeychainUnavailable(
                f"cannot establish Keychain service absence (security exit {result.returncode}): "
                f"{result.stderr.decode(errors='replace').strip()[:180]}"
            )

    def read(self):
        result = self.command("find-generic-password", "-s", self.service,
            "-a", self.account, "-w")
        if result.returncode != 0:
            raise KeychainUnavailable(
                f"cannot read isolated Keychain item (security exit {result.returncode}): "
                f"{result.stderr.decode(errors='replace').strip()[:180]}"
            )
        return json.loads(result.stdout)

    def create(self, credential):
        self.assert_absent()
        payload = json.dumps(credential).encode()
        # No -U: an item appearing between the absence check and this add
        # causes failure rather than replacing somebody else's item.
        try:
            result = self.command("add-generic-password", "-a", self.account,
                "-s", self.service, "-l", self.label, "-X", payload.hex())
        except KeychainUnavailable:
            # A timed-out add might have completed before security stalled.
            # Mark it ours for finally cleanup only if its unique label can
            # be observed through an attribute-only lookup.
            try:
                probe = self.command("find-generic-password", "-s", self.service,
                    "-a", self.account)
                attributes = probe.stdout + probe.stderr
                if probe.returncode == 0 and self.label.encode() in attributes:
                    self.created = True
            except KeychainUnavailable:
                pass
            raise
        if result.returncode != 0:
            raise KeychainUnavailable(
                f"cannot create isolated Keychain item (security exit {result.returncode}): "
                f"{result.stderr.decode(errors='replace').strip()[:180]}"
            )
        self.created = True
        if self.read() != credential:
            raise KeychainUnavailable("isolated Keychain item did not read back as written")

    def write(self, credential):
        assert self.created, "only an item created by this fixture may be updated"
        payload = json.dumps(credential).encode()
        # Match Swapdex's security -i write path; synthetic tokens stay off
        # the command line. This updates only the item we just created.
        command = (f'add-generic-password -U -a "{self.account}" '
            f'-s "{self.service}" -l "{self.label}" -X {payload.hex()}\n').encode()
        result = self.command("-i", input=command)
        if result.returncode != 0 or self.read() != credential:
            raise KeychainUnavailable(
                f"cannot update isolated Keychain item (security exit {result.returncode})"
            )

    def cleanup(self):
        if not self.created:
            return
        result = self.command("delete-generic-password", "-s", self.service,
            "-a", self.account)
        if result.returncode != 0:
            raise KeychainUnavailable(
                f"cannot delete isolated Keychain item (security exit {result.returncode}): "
                f"{result.stderr.decode(errors='replace').strip()[:180]}"
            )
        self.created = False


class FixtureServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        self.expected_access = "fixture-access-2"
        self.requests = []
        self.lock = threading.Lock()
        super().__init__(("127.0.0.1", 0), FixtureHandler)


class FixtureHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        auth = self.headers.get("Authorization", "")
        with self.server.lock:
            expected = self.server.expected_access
            self.server.requests.append(
                {"path": self.path, "expected_access": auth == f"Bearer {expected}"}
            )
        if "count_tokens" in self.path:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"input_tokens":1}')
            return
        if urlsplit(self.path).path.rstrip("/") != "/v1/messages" or auth != f"Bearer {expected}":
            self.send_response(401)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(
                b'{"type":"error","error":{"type":"authentication_error","message":"fixture access mismatch"}}'
            )
            return
        request = json.loads(body)
        message = {
            "id": "msg_fixture",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": "fixture-ok"}],
            "stop_reason": "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }
        self.send_response(200)
        if request.get("stream"):
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            events = [
                ("message_start", {"type": "message_start", "message": dict(message, content=[], stop_reason=None)}),
                ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "fixture-ok"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}}),
                ("message_stop", {"type": "message_stop"}),
            ]
            for name, value in events:
                self.wfile.write(f"event: {name}\ndata: {json.dumps(value)}\n\n".encode())
                self.wfile.flush()
        else:
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(message).encode())


FAKE_CURL = r'''#!PYTHON
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["SWAPDEX_TEST_FIXTURE_ROOT"])
config = sys.stdin.read()
data_line = next(line for line in config.splitlines() if line.startswith("data = "))
body = json.loads(json.loads(data_line.split("=", 1)[1].strip()))
token = body["refresh_token"]
round_number = {"fixture-refresh-1": 1, "fixture-refresh-2": 2}.get(token)
with (root / "oauth-spends.jsonl").open("a") as log:
    log.write(json.dumps({"round": round_number, "known_fixture_token": round_number is not None}) + "\n")
if round_number is None:
    sys.exit("unexpected fixture refresh token")
(root / f"refresh-{round_number}.started").touch()
deadline = time.monotonic() + 30
while not (root / f"refresh-{round_number}.release").exists():
    if time.monotonic() >= deadline:
        sys.exit("fixture refresh release timed out")
    time.sleep(.02)
sys.stdout.write(json.dumps({"access_token": f"fixture-access-{round_number + 1}",
                             "refresh_token": f"fixture-refresh-{round_number + 1}",
                             "expires_in": 4 if round_number == 1 else 3600}) + "\n200")
'''


def stop_child(child):
    if child is None:
        return
    if child.poll() is None:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait()


def wait_for(path, child, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        if child.poll() is not None:
            stdout, stderr = child.communicate()
            raise AssertionError(
                f"swapdex refresh exited {child.returncode} before {path.name}: "
                f"{stdout.decode(errors='replace')[:300]} {stderr.decode(errors='replace')[:300]}"
            )
        time.sleep(.02)
    raise AssertionError(f"timed out waiting for {path.name}")


def next_result(child, pending, timeout):
    deadline = time.monotonic() + timeout
    with selectors.DefaultSelector() as selector:
        selector.register(child.stdout, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            if selector.select(.1):
                part = os.read(child.stdout.fileno(), 65536)
                if not part:
                    raise AssertionError("native process closed stream-json output")
                pending += part
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    try:
                        event = json.loads(line)
                    except ValueError:
                        continue
                    if event.get("type") == "result":
                        return event, pending
            if child.poll() is not None:
                raise AssertionError("native process exited before turn result")
    return None, pending


def send_turn(child, session_id, number):
    event = {
        "type": "user",
        "message": {"role": "user", "content": f"fixture request {number}"},
        "parent_tool_use_id": None,
        "session_id": session_id,
    }
    child.stdin.write((json.dumps(event) + "\n").encode())
    child.stdin.flush()


def model_requests(server):
    with server.lock:
        return [request for request in server.requests if "count_tokens" not in request["path"]]


def run(args):
    server = FixtureServer()
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    native = None
    refresher = None
    keychain = None
    report = {"result": "FAIL", "claude": str(args.claude), "swapdex": str(args.swapdex)}
    try:
        home_context = (contextlib.nullcontext(str(args.fixture_home)) if args.fixture_home
            else tempfile.TemporaryDirectory(prefix="swapdex-claude-renewal-"))
        with home_context as temporary:
            root = Path(temporary)
            if args.fixture_home:
                assert root.is_dir() and not root.is_symlink(), "fixture HOME must be a real directory"
                assert not any(root.iterdir()), "fixture HOME must be empty"
            store = (root / "Library/Application Support/swapdex" if args.keychain
                else root / ".local/share/swapdex")
            slot = store / "slots/account"
            account = os.environ.get("USER") or os.environ.get("LOGNAME") or "claude-code-user"
            common = {
                "HOME": temporary, "USER": account, "LOGNAME": account,
                "PATH": "/usr/bin:/bin", "TERM": "xterm-256color",
                "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9",
                "http_proxy": "http://127.0.0.1:9", "https_proxy": "http://127.0.0.1:9",
                "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost",
            }
            if args.keychain:
                assert sys.platform == "darwin", "--keychain requires macOS"
                assert account and all(char in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-"
                    for char in account), "Keychain fixture requires a simple local username"
                assert store == root / "Library/Application Support/swapdex"
                assert slot.is_relative_to(root)
                keychain = KeychainFixture(slot, account, common)
                report["keychain_service"] = keychain.service
                # Print the exact unrooted path and service BEFORE creating
                # the registry or touching Keychain state. The native and
                # Swapdex children receive this same HOME and slot path.
                print(json.dumps({"preflight": {"home": temporary,
                    "store_dir": str(store), "slot_dir": str(slot),
                    "keychain_service": keychain.service,
                    "swapdex_root_unset": True}}), flush=True)
                if args.preflight_only:
                    report["result"] = "PREFLIGHT"
                    return report["result"]
                keychain.assert_absent()
            slot.mkdir(parents=True)
            registry = store / "slots.json"
            registry.write_text(json.dumps([{
                "name": "work", "id": "account", "config_dir": str(slot),
                "adopted": False, "tool": "claude-code",
            }]))
            identity = {
                "hasCompletedOnboarding": True,
                "oauthAccount": {
                    "accountUuid": "fixture-account", "organizationUuid": "fixture-org",
                    "emailAddress": "fixture@example.com",
                },
                "projects": {temporary: {"hasTrustDialogAccepted": True}},
            }
            (slot / ".claude.json").write_text(json.dumps(identity))
            credential_path = slot / ".credentials.json"
            empty_credential = {"claudeAiOauth": {
                "accessToken": "", "refreshToken": "", "expiresAt": 0,
                "refreshTokenExpiresAt": int(time.time() * 1000) + 86_400_000,
                "scopes": ["user:inference", "user:profile"], "subscriptionType": "max",
            }}
            if keychain:
                keychain.create(empty_credential)
            else:
                credential_path.write_text(json.dumps(empty_credential))
                credential_path.chmod(0o600)
            fake_curl = root / "fake-curl"
            fake_curl.write_text(FAKE_CURL.replace("#!PYTHON", f"#!{sys.executable}"))
            fake_curl.chmod(0o700)
            native_env = dict(common, CLAUDE_CONFIG_DIR=str(slot),
                ANTHROPIC_BASE_URL=f"http://127.0.0.1:{server.server_port}",
                CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1",
                DISABLE_TELEMETRY="1", DISABLE_ERROR_REPORTING="1")
            # The installed command is named `claude` even though its target is
            # a versioned binary. Linux /proc/comm follows the invoked name;
            # native ownership discovery intentionally requires that name.
            native_command_path = root / "claude"
            native_command_path.symlink_to(args.claude.resolve())
            native_command = [str(native_command_path), "-p", "--input-format", "stream-json",
                "--output-format", "stream-json", "--verbose", "--model", "claude-sonnet-4-6",
                "--tools", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
                "--setting-sources", "", "--no-session-persistence"]
            refresh_env = dict(common, CLAUDE_CONFIG_DIR=str(slot),
                SWAPDEX_CURL=str(fake_curl), SWAPDEX_TEST_FIXTURE_ROOT=temporary)
            if not keychain:
                refresh_env.update(SWAPDEX_ROOT=temporary,
                    SWAPDEX_OAUTH_URL="http://127.0.0.1:1/oauth/token")
            with (root / "native-stderr").open("wb") as native_stderr:
                native = subprocess.Popen(native_command, cwd=root, env=native_env,
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=native_stderr,
                    start_new_session=True)
                report["native_pid"] = native.pid
                # Start without a usable login so native startup cannot begin
                # an uncontrolled OAuth exchange before Swapdex has the pair.
                time.sleep(.7)
                assert native.poll() is None, "native exited during fixture initialization"
                first_credential = (keychain.read() if keychain
                    else json.loads(credential_path.read_text()))
                first_credential["claudeAiOauth"].update(
                    accessToken="fixture-access-1", refreshToken="fixture-refresh-1",
                    expiresAt=int(time.time() * 1000) - 60_000,
                )
                if keychain:
                    keychain.write(first_credential)
                else:
                    initial_replacement = root / "credential-initial.tmp"
                    initial_replacement.write_text(json.dumps(first_credential))
                    initial_replacement.chmod(0o600)
                    initial_replacement.replace(credential_path)
                session_id = str(uuid.uuid4())
                pending = b""
                for number in (1, 2):
                    if number == 2:
                        # Let the short first access lifetime lapse in the
                        # live process as well as on disk before the next turn.
                        time.sleep(5)
                        current = (keychain.read() if keychain
                            else json.loads(credential_path.read_text()))
                        current["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) - 1
                        if keychain:
                            keychain.write(current)
                        else:
                            replacement = root / "credential-replacement.tmp"
                            replacement.write_text(json.dumps(current))
                            replacement.chmod(0o600)
                            replacement.replace(credential_path)
                        with server.lock:
                            server.expected_access = "fixture-access-3"
                    refresher = subprocess.Popen([str(args.swapdex), "refresh", "work"],
                        cwd=root, env=refresh_env, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, start_new_session=True)
                    wait_for(root / f"refresh-{number}.started", refresher)
                    before = len(model_requests(server))
                    send_turn(native, session_id, number)
                    early, pending = next_result(native, pending, 1)
                    assert early is None, f"native turn {number} returned before lock release"
                    assert len(model_requests(server)) == before, (
                        f"native turn {number} reached model before lock release"
                    )
                    (root / f"refresh-{number}.release").touch()
                    stdout, stderr = refresher.communicate(timeout=25)
                    assert refresher.returncode == 0, (
                        f"swapdex refresh {number} failed: {stdout.decode(errors='replace')[:250]} "
                        f"{stderr.decode(errors='replace')[:250]}"
                    )
                    refresher = None
                    result, pending = next_result(native, pending, 45)
                    assert result is not None, (
                        f"native turn {number} timed out; model requests={model_requests(server)}; "
                        f"native stderr={(root / 'native-stderr').read_text(errors='replace')[-400:]}"
                    )
                    assert not result.get("is_error") and result.get("result") == "fixture-ok", (
                        f"native turn {number} failed after refresh"
                    )
                    assert native.poll() is None, "native process exited between turns"
                    credential = (keychain.read() if keychain
                        else json.loads(credential_path.read_text()))["claudeAiOauth"]
                    assert credential["refreshToken"] == f"fixture-refresh-{number + 1}"
                    requests = model_requests(server)
                    assert len(requests) == number and requests[-1]["expected_access"], (
                        f"native turn {number} did not use the renewed bearer"
                    )
                spends = [json.loads(line) for line in (root / "oauth-spends.jsonl").read_text().splitlines()]
                assert [item["round"] for item in spends] == [1, 2], "fixture refresh token spent more than once"
                report.update(result="PASS", same_native_process=True, oauth_spends=[1, 2],
                    model_requests=len(model_requests(server)),
                    credential_source="keychain" if keychain else "file")
    except KeychainUnavailable as error:
        report.update(result="BLOCKED", error=str(error))
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        stop_child(refresher)
        stop_child(native)
        if keychain:
            try:
                keychain.cleanup()
            except KeychainUnavailable as error:
                report.update(result="FAIL", cleanup_error=str(error))
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)
        if args.output:
            args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report), flush=True)
    return report["result"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--swapdex", type=Path, required=True, help="candidate native Swapdex executable")
    parser.add_argument("--claude", type=Path, required=True, help="stock Claude executable")
    parser.add_argument("--keychain", action="store_true",
        help="use one newly created, random-slot-derived macOS Keychain item")
    parser.add_argument("--fixture-home", type=Path,
        help="existing empty temporary HOME; caller removes it after the run")
    parser.add_argument("--preflight-only", action="store_true",
        help="print isolated macOS paths and Keychain service without state writes")
    parser.add_argument("--output", type=Path, help="optional JSON report path")
    args = parser.parse_args()
    if (args.fixture_home or args.preflight_only) and not args.keychain:
        parser.error("--fixture-home and --preflight-only require --keychain")
    for path in (args.swapdex, args.claude):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"not an executable file: {path}")
    args.swapdex = args.swapdex.resolve()
    args.claude = args.claude.resolve()
    result = run(args)
    if result not in ("PASS", "PREFLIGHT"):
        sys.exit(2 if result == "BLOCKED" else 1)


if __name__ == "__main__":
    main()
