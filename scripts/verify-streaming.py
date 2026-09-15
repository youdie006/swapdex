#!/usr/bin/env python3
"""Verify that an installed Swapdex binary relays SSE without buffering.

The fixture uses fake credentials, an isolated SWAPDEX_ROOT, and loopback-only
HTTP servers.  Its upstream waits for the downstream client to acknowledge
each flushed stage, proving that bytes arrived before upstream EOF.
"""

import argparse
import errno
import http.server
import importlib.util
import json
import math
import os
from pathlib import Path
import socket
import tempfile
import threading
import time


HELPER_PATH = Path(__file__).with_name("verify-installed-account-routing.py")
STAGE_TIMEOUT = 3.0
GATE_TIMEOUT = 6.0
SOCKET_POLL = 0.1
PING = b": fixture-ping\n\n"
COMPLETIONS = {
    "claude": b'event: message_stop\ndata: {"type":"message_stop"}\n\n',
    "codex": b'event: response.completed\ndata: {"type":"response.completed"}\n\n',
}


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def load_routing_helper():
    require(HELPER_PATH.is_file(), "installed-routing helper is missing")
    spec = importlib.util.spec_from_file_location("swapdex_installed_routing", HELPER_PATH)
    require(spec is not None and spec.loader is not None,
            "could not load installed-routing helper")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


routing = load_routing_helper()


def read_request_body(handler):
    if handler.headers.get("Transfer-Encoding", "").lower() != "chunked":
        return handler.rfile.read(int(handler.headers.get("Content-Length", "0")))
    body = bytearray()
    while True:
        size_line = handler.rfile.readline()
        require(size_line, "fixture request ended inside chunk framing")
        size = int(size_line.split(b";", 1)[0], 16)
        if size == 0:
            while handler.rfile.readline() not in (b"\r\n", b"\n", b""):
                pass
            return bytes(body)
        body.extend(handler.rfile.read(size))
        require(handler.rfile.read(2) == b"\r\n", "invalid fixture request chunk")


class ControlledServer(http.server.ThreadingHTTPServer):
    daemon_threads = False
    block_on_close = True

    def __init__(self, mode, completion, hold_seconds=0.0):
        super().__init__(("127.0.0.1", 0), ControlledHandler)
        self.mode = mode
        self.completion = completion
        self.hold_seconds = hold_seconds
        self.abort = threading.Event()
        self.announced = {
            stage: threading.Event()
            for stage in ("headers", "ping", "completion", "malformed", "end")
        }
        self.released = {
            stage: threading.Event()
            for stage in ("headers", "ping", "completion", "malformed")
        }
        self.lock = threading.Lock()
        self.records = []
        self.errors = []
        self.sent_body = bytearray()
        self.heartbeat_count = 0

    def record(self, path, body):
        with self.lock:
            self.records.append((path, body))

    def record_error(self, error):
        with self.lock:
            self.errors.append(f"{type(error).__name__}: {error}")

    def record_fragment(self, fragment, heartbeat=False):
        with self.lock:
            self.sent_body.extend(fragment)
            if heartbeat:
                self.heartbeat_count += 1

    def snapshot(self):
        with self.lock:
            return (list(self.records), list(self.errors), bytes(self.sent_body),
                    self.heartbeat_count)

    def gate(self, stage):
        self.announced[stage].set()
        deadline = time.monotonic() + GATE_TIMEOUT
        while not self.abort.is_set():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"fixture timed out waiting for {stage} acknowledgement")
            if self.released[stage].wait(min(SOCKET_POLL, remaining)):
                return True
        return False

    def release_all(self):
        self.abort.set()
        for event in self.released.values():
            event.set()


class ControlledHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(STAGE_TIMEOUT)

    def write_chunk(self, data):
        self.wfile.write(f"{len(data):x}\r\n".encode("ascii"))
        self.wfile.write(data)
        self.wfile.write(b"\r\n")
        self.wfile.flush()

    def do_POST(self):
        try:
            body = read_request_body(self)
            self.server.record(self.path, body)
            connection = (b"close" if self.server.mode in ("stream", "hold")
                          else b"keep-alive")
            self.wfile.write(
                b"HTTP/1.1 200 OK\r\n"
                b"Content-Type: Text/Event-Stream; charset=utf-8\r\n"
                b"X-Stream-Fixture: preserved\r\n"
                b"Transfer-Encoding: chunked\r\n"
                b"Connection: " + connection + b"\r\n\r\n"
            )
            self.wfile.flush()
            if not self.server.gate("headers"):
                return

            self.server.record_fragment(PING, heartbeat=True)
            self.write_chunk(PING)
            if not self.server.gate("ping"):
                return

            if self.server.mode == "hold":
                deadline = time.monotonic() + self.server.hold_seconds
                sequence = 1
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        break
                    if self.server.abort.wait(min(1.0, remaining)):
                        return
                    if time.monotonic() >= deadline:
                        break
                    heartbeat = f": fixture-heartbeat-{sequence}\n\n".encode("ascii")
                    self.server.record_fragment(heartbeat, heartbeat=True)
                    self.write_chunk(heartbeat)
                    sequence += 1

            if self.server.mode in ("stream", "hold"):
                self.server.record_fragment(self.server.completion)
                self.write_chunk(self.server.completion)
                if not self.server.gate("completion"):
                    return
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
                self.server.announced["end"].set()
            else:
                self.wfile.write(b"not-hex\r\n")
                self.wfile.flush()
                if not self.server.gate("malformed"):
                    return
        except (BrokenPipeError, ConnectionResetError, TimeoutError, OSError) as error:
            if not self.server.abort.is_set():
                self.server.record_error(error)
        except BaseException as error:
            self.server.record_error(error)
        finally:
            self.close_connection = True


def start_server(mode, completion, hold_seconds=0.0):
    server = ControlledServer(mode, completion, hold_seconds)
    thread = threading.Thread(
        target=server.serve_forever,
        name=f"swapdex-stream-{mode}-upstream",
        daemon=False,
    )
    thread.start()
    return server, thread


def stop_server(server, thread):
    failures = []
    server.release_all()
    try:
        server.shutdown()
    except BaseException as error:
        failures.append(f"shutdown: {error}")
    try:
        server.server_close()
    except BaseException as error:
        failures.append(f"close: {error}")
    thread.join(timeout=STAGE_TIMEOUT)
    if thread.is_alive():
        failures.append("serve thread remained alive")
    require(not failures, "fixture cleanup failed: " + "; ".join(failures))


def wait_for_stage(server, stage):
    require(server.announced[stage].wait(STAGE_TIMEOUT),
            f"upstream did not flush the {stage} stage")


def split_response(wire):
    boundary = wire.find(b"\r\n\r\n")
    if boundary < 0:
        return None, {}, b""
    lines = bytes(wire[:boundary]).split(b"\r\n")
    parts = lines[0].split(b" ", 2)
    require(len(parts) >= 2 and parts[1].isdigit(), "invalid downstream status line")
    headers = {}
    for line in lines[1:]:
        require(b":" in line, "invalid downstream response header")
        name, value = line.split(b":", 1)
        headers.setdefault(name.strip().lower(), []).append(value.strip())
    return int(parts[1]), headers, wire[boundary + 4:]


def decode_body(wire):
    status, headers, encoded = split_response(wire)
    if status is None:
        return b"", False
    transfer = b",".join(headers.get(b"transfer-encoding", [])).lower()
    require(b"chunked" in transfer, "downstream SSE response was not chunked")
    body = bytearray()
    offset = 0
    while True:
        line_end = encoded.find(b"\r\n", offset)
        if line_end < 0:
            return bytes(body), False
        size_text = encoded[offset:line_end].split(b";", 1)[0]
        try:
            size = int(size_text, 16)
        except ValueError as error:
            raise AssertionError("downstream emitted invalid chunk framing") from error
        offset = line_end + 2
        if size == 0:
            trailer_end = encoded.find(b"\r\n", offset)
            if trailer_end < 0:
                return bytes(body), False
            return bytes(body), True
        if len(encoded) < offset + size + 2:
            return bytes(body), False
        body.extend(encoded[offset:offset + size])
        require(encoded[offset + size:offset + size + 2] == b"\r\n",
                "downstream chunk lacked its delimiter")
        offset += size + 2


def receive_until(client, wire, predicate, description):
    deadline = time.monotonic() + STAGE_TIMEOUT
    while not predicate(wire):
        remaining = deadline - time.monotonic()
        require(remaining > 0, f"downstream withheld {description} while upstream stayed open")
        client.settimeout(min(SOCKET_POLL, remaining))
        try:
            fragment = client.recv(16384)
        except socket.timeout:
            continue
        require(fragment, f"downstream closed before delivering {description}")
        wire.extend(fragment)


def receive_close(client, wire, description):
    deadline = time.monotonic() + STAGE_TIMEOUT
    while time.monotonic() < deadline:
        client.settimeout(min(SOCKET_POLL, deadline - time.monotonic()))
        try:
            fragment = client.recv(16384)
        except socket.timeout:
            continue
        except ConnectionResetError:
            return
        except OSError as error:
            if error.errno in (errno.ECONNRESET, errno.ENOTCONN):
                return
            raise
        if not fragment:
            return
        wire.extend(fragment)
    raise AssertionError(f"downstream did not {description} promptly")


def request_for(tool):
    if tool == "claude":
        path = "/v1/messages"
        body = json.dumps({
            "model": "fixture",
            "messages": [],
            "metadata": {"user_id": json.dumps({"account_uuid": "fixture-uuid-a"})},
        }, separators=(",", ":")).encode("utf-8")
        headers = [b"Authorization: Bearer fixture-client"]
    else:
        path = "/v1/responses"
        body = b'{"input":[]}'
        headers = [
            b"Authorization: Bearer fixture-client",
            b"chatgpt-account-id: fixture-client",
        ]
    request = [
        f"POST {path} HTTP/1.1".encode("ascii"),
        b"Host: 127.0.0.1",
        b"Content-Type: application/json",
        f"Content-Length: {len(body)}".encode("ascii"),
        *headers,
    ]
    return path, body, request


def expected_upstream_path(tool, client_path):
    # Codex intentionally removes the public /v1 prefix for its backend URL.
    return "/responses" if tool == "codex" else client_path


def check_headers(wire, tool):
    status, headers, _ = split_response(wire)
    require(status == 200, f"{tool} downstream status was {status}, expected 200")
    content_type = b",".join(headers.get(b"content-type", [])).lower()
    require(content_type.startswith(b"text/event-stream"),
            f"{tool} downstream lost the SSE content type")
    fixture = headers.get(b"x-stream-fixture", [])
    require(fixture == [b"preserved"], f"{tool} downstream lost an end-to-end header")
    return status


def open_request(port, tool, persistent):
    path, body, lines = request_for(tool)
    connection = b"keep-alive" if persistent else b"close"
    wire_request = b"\r\n".join([*lines, b"Connection: " + connection]) + b"\r\n\r\n" + body
    client = socket.create_connection(("127.0.0.1", port), timeout=STAGE_TIMEOUT)
    try:
        client.sendall(wire_request)
    except BaseException:
        client.close()
        raise
    return client, path, body


def cleanup_case(client, proxy, server, thread):
    failures = []
    server.release_all()
    if client is not None:
        try:
            client.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            client.close()
        except BaseException as error:
            failures.append(f"client close: {error}")
    if proxy is not None:
        try:
            routing.stop_process(proxy)
        except BaseException as error:
            failures.append(f"proxy stop: {error}")
    try:
        stop_server(server, thread)
    except BaseException as error:
        failures.append(f"server stop: {error}")
    require(not failures, "case cleanup failed: " + "; ".join(failures))


def verify_stream(swapdex, root, env, tool):
    expected = PING + COMPLETIONS[tool]
    server, thread = start_server("stream", COMPLETIONS[tool])
    proxy = None
    client = None
    wire = bytearray()
    try:
        proxy, port, _ = routing.start_proxy(swapdex, root, env, tool, server)
        client, path, request_body = open_request(port, tool, persistent=False)

        wait_for_stage(server, "headers")
        receive_until(client, wire, lambda data: b"\r\n\r\n" in data,
                      f"{tool} SSE headers")
        status = check_headers(wire, tool)
        require(not server.announced["end"].is_set(),
                f"{tool} headers arrived only after upstream EOF")
        server.released["headers"].set()

        wait_for_stage(server, "ping")
        receive_until(client, wire, lambda data: decode_body(data)[0] == PING,
                      f"{tool} SSE ping")
        require(not server.announced["end"].is_set(),
                f"{tool} ping arrived only after upstream EOF")
        server.released["ping"].set()

        wait_for_stage(server, "completion")
        receive_until(client, wire, lambda data: decode_body(data)[0] == expected,
                      f"{tool} completion event")
        require(not server.announced["end"].is_set(),
                f"{tool} completion arrived only after upstream EOF")
        server.released["completion"].set()

        wait_for_stage(server, "end")
        receive_close(client, wire, "finish the SSE response")
        body, complete = decode_body(wire)
        require(complete, f"{tool} downstream omitted the successful end-of-stream marker")
        require(body == expected, f"{tool} changed SSE response body bytes")
        records, errors, _sent_body, _heartbeat_count = server.snapshot()
        require(not errors, f"{tool} upstream fixture failed: {'; '.join(errors)}")
        require(len(records) == 1, f"{tool} stream made {len(records)} upstream requests")
        require(records[0] == (expected_upstream_path(tool, path), request_body),
                f"{tool} stream changed the upstream request path or body")
        return {
            "status": status,
            "headers_before_eof": True,
            "ping_before_eof": True,
            "completion_before_eof": True,
            "body_bytes": len(body),
            "end_stream": True,
            "upstream_requests": len(records),
        }
    finally:
        cleanup_case(client, proxy, server, thread)
        require(not routing.marker_for(root, tool).exists(),
                f"{tool} proxy marker remained after stream cleanup")


def verify_malformed(swapdex, root, env, tool):
    server, thread = start_server("malformed", COMPLETIONS[tool])
    proxy = None
    client = None
    wire = bytearray()
    try:
        proxy, port, _ = routing.start_proxy(swapdex, root, env, tool, server)
        client, path, request_body = open_request(port, tool, persistent=True)

        wait_for_stage(server, "headers")
        receive_until(client, wire, lambda data: b"\r\n\r\n" in data,
                      f"{tool} malformed-case headers")
        status = check_headers(wire, tool)
        server.released["headers"].set()

        wait_for_stage(server, "ping")
        receive_until(client, wire, lambda data: decode_body(data)[0] == PING,
                      f"{tool} malformed-case ping")
        server.released["ping"].set()

        wait_for_stage(server, "malformed")
        receive_close(client, wire, "close the interrupted persistent response")
        body, complete = decode_body(wire)
        require(body == PING, f"{tool} changed bytes before the upstream framing error")
        require(not complete,
                f"{tool} converted an upstream framing error into successful end-of-stream")
        server.released["malformed"].set()
        records, errors, _sent_body, _heartbeat_count = server.snapshot()
        require(not errors, f"{tool} malformed fixture failed: {'; '.join(errors)}")
        require(len(records) == 1, f"{tool} malformed case made {len(records)} upstream requests")
        require(records[0] == (expected_upstream_path(tool, path), request_body),
                f"{tool} malformed case changed the upstream request path or body")
        return {
            "status": status,
            "body_bytes": len(body),
            "prompt_close": True,
            "terminal_success_chunk": False,
            "upstream_requests": len(records),
        }
    finally:
        cleanup_case(client, proxy, server, thread)
        require(not routing.marker_for(root, tool).exists(),
                f"{tool} proxy marker remained after malformed-case cleanup")


def receive_while_holding(client, wire, server, hold_seconds):
    deadline = time.monotonic() + hold_seconds + STAGE_TIMEOUT
    pending_since = None
    while True:
        _, errors, expected, heartbeat_count = server.snapshot()
        require(not errors, "long-hold upstream fixture failed: " + "; ".join(errors))
        body, complete = decode_body(wire)
        require(not complete, "long-hold downstream ended before upstream completion")
        require(expected.startswith(body), "long-hold downstream changed SSE response bytes")
        if body == expected:
            pending_since = None
            if server.announced["completion"].is_set():
                return expected, heartbeat_count
        elif pending_since is None:
            pending_since = time.monotonic()
        if pending_since is not None:
            require(time.monotonic() - pending_since < STAGE_TIMEOUT,
                    "long-hold downstream withheld a flushed heartbeat")
        remaining = deadline - time.monotonic()
        require(remaining > 0, "long-hold upstream did not reach its completion stage")
        client.settimeout(min(SOCKET_POLL, remaining))
        try:
            fragment = client.recv(16384)
        except socket.timeout:
            continue
        require(fragment, "long-hold downstream closed before upstream completion")
        wire.extend(fragment)


def verify_long_hold(swapdex, root, env, hold_seconds):
    tool = "claude"
    server, thread = start_server("hold", COMPLETIONS[tool], hold_seconds)
    proxy = None
    client = None
    wire = bytearray()
    started = None
    try:
        proxy, port, _ = routing.start_proxy(swapdex, root, env, tool, server)
        client, path, request_body = open_request(port, tool, persistent=False)

        wait_for_stage(server, "headers")
        receive_until(client, wire, lambda data: b"\r\n\r\n" in data,
                      "long-hold SSE headers")
        status = check_headers(wire, tool)
        server.released["headers"].set()

        wait_for_stage(server, "ping")
        receive_until(client, wire, lambda data: decode_body(data)[0] == PING,
                      "long-hold first heartbeat")
        started = time.monotonic()
        server.released["ping"].set()

        expected, heartbeat_count = receive_while_holding(
            client, wire, server, hold_seconds)
        require(not server.announced["end"].is_set(),
                "long-hold completion arrived only after upstream EOF")
        server.released["completion"].set()

        wait_for_stage(server, "end")
        receive_close(client, wire, "finish the long-hold SSE response")
        elapsed = time.monotonic() - started
        body, complete = decode_body(wire)
        require(complete, "long-hold downstream omitted its end-of-stream marker")
        require(body == expected, "long-hold downstream changed SSE response bytes")
        require(elapsed >= hold_seconds,
                "long-hold completion arrived before the requested interval")
        records, errors, sent_body, sent_heartbeats = server.snapshot()
        require(not errors, "long-hold upstream fixture failed: " + "; ".join(errors))
        require(sent_body == expected and sent_heartbeats == heartbeat_count,
                "long-hold fixture observations changed after completion")
        require(len(records) == 1,
                f"long-hold stream made {len(records)} upstream requests")
        require(records[0] == (expected_upstream_path(tool, path), request_body),
                "long-hold stream changed the upstream request path or body")
        return {
            "tool": tool,
            "status": status,
            "requested_seconds": hold_seconds,
            "elapsed_seconds": round(elapsed, 3),
            "heartbeat_count": heartbeat_count,
            "body_bytes": len(body),
            "completion_before_eof": True,
            "end_stream": True,
            "upstream_requests": len(records),
            "exceeds_legacy_300s_deadline": hold_seconds > 300.0,
        }
    finally:
        cleanup_case(client, proxy, server, thread)
        require(not routing.marker_for(root, tool).exists(),
                "Claude proxy marker remained after long-hold cleanup")


def binary_version(swapdex, env):
    _, stdout, _ = routing.run([swapdex, "--version"], env, timeout=10)
    lines = stdout.decode("utf-8", errors="replace").splitlines()
    require(lines, "Swapdex did not report a version")
    return lines[0][:120]


def verify(swapdex, hold_seconds):
    with tempfile.TemporaryDirectory(prefix="swapdex-streaming-") as temporary:
        root = Path(temporary)
        native_bin = root / "native-bin"
        native_bin.mkdir()
        routing.write_fake_client(native_bin / "claude", "claude")
        routing.write_fake_client(native_bin / "codex", "codex")
        env = routing.safe_env(root, native_bin)
        routing.seed_accounts(swapdex, root, env)
        version = binary_version(swapdex, env)
        tools = {}
        for tool in ("claude", "codex"):
            tools[tool] = {
                "stream": verify_stream(swapdex, root, env, tool),
                "malformed": verify_malformed(swapdex, root, env, tool),
            }
        result = {"status": "pass", "version": version, "tools": tools}
        if hold_seconds:
            result["long_hold"] = verify_long_hold(
                swapdex, root, env, hold_seconds)
        return result


def safe_error(error):
    text = " ".join(str(error).split())
    return {"type": type(error).__name__, "message": text[:500]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--swapdex", required=True,
                        help="absolute path to the installed native executable")
    parser.add_argument(
        "--hold-seconds",
        type=float,
        default=0.0,
        help="also hold one Claude SSE stream open with one-second heartbeats; "
             "use more than 300 seconds to cover the former body deadline",
    )
    arguments = parser.parse_args()
    try:
        supplied = Path(arguments.swapdex)
        require(supplied.is_absolute(), "--swapdex must be an absolute path")
        swapdex = supplied.resolve()
        require(swapdex.is_file() and os.access(swapdex, os.X_OK),
                "--swapdex is not executable")
        require(math.isfinite(arguments.hold_seconds) and arguments.hold_seconds >= 0.0,
                "--hold-seconds must be a finite non-negative number")
        result = verify(str(swapdex), arguments.hold_seconds)
    except Exception as error:
        print(json.dumps({"status": "fail", "error": safe_error(error)},
                         sort_keys=True), flush=True)
        return 1
    print(json.dumps(result, sort_keys=True), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
