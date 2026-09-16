#!/usr/bin/env python3
"""Exercise the slot quickstart with fake native logins and a local provider."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import selectors
import signal
import socket
import subprocess
import sys
import tempfile
import time


HELPER_PATH = Path(__file__).with_name("verify-installed-account-routing.py")
SPEC = importlib.util.spec_from_file_location("installed_routing", HELPER_PATH)
routing = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(routing)

FAKE_NATIVE = r'''
import http.client
import json
import os
from pathlib import Path
import sys
from urllib.parse import urlsplit

tool = Path(sys.argv[0]).name
home_key = "CLAUDE_CONFIG_DIR" if tool == "claude" else "CODEX_HOME"
home = Path(os.environ[home_key])
args = sys.argv[1:]
login = args[:2] == ["auth", "login"] if tool == "claude" else args[:1] == ["login"]
if login:
    name = os.environ["SWAPDEX_FIXTURE_ACCOUNT"]
    home.mkdir(parents=True, exist_ok=True)
    if tool == "claude":
        (home / ".credentials.json").write_text(json.dumps({"claudeAiOauth": {
            "accessToken": "fixture-claude-" + name,
            "refreshToken": "fixture-refresh-" + name,
            "expiresAt": 4102444800000,
        }}))
        (home / ".claude.json").write_text(json.dumps({"oauthAccount": {
            "accountUuid": "fixture-uuid-" + name,
            "emailAddress": name + "@example.com",
        }}))
    else:
        assert args == ["login", "--device-auth"], args
        (home / "auth.json").write_text(json.dumps({"auth_mode": "chatgpt", "tokens": {
            "access_token": "fixture-codex-" + name,
            "refresh_token": "fixture-refresh-" + name,
            "account_id": "fixture-account-" + name,
        }}))
    print(json.dumps({"event": "login", "home": str(home)}), flush=True)
    sys.exit(0)

base = os.environ.get("ANTHROPIC_BASE_URL", "") if tool == "claude" else ""
if tool == "codex":
    for item in args:
        if item.startswith("openai_base_url="):
            base = item.split("=", 1)[1].strip('"')
url = urlsplit(base)
assert url.scheme == "http" and url.hostname == "127.0.0.1" and url.port, base
connection = http.client.HTTPConnection(url.hostname, url.port, timeout=10)
print(json.dumps({"event": "ready", "home": str(home), "pid": os.getpid()}), flush=True)
try:
    for line in sys.stdin:
        if line.strip() == "quit":
            break
        assert line.strip() == "turn"
        path = "/v1/messages" if tool == "claude" else "/v1/responses"
        connection.request("POST", path, json.dumps({"model": "fixture", "input": []}), {
            "Content-Type": "application/json", "Authorization": "Bearer fixture-client",
            "chatgpt-account-id": "fixture-client",
        })
        response = connection.getresponse()
        body = json.loads(response.read())
        print(json.dumps({"event": "turn", "status": response.status, "body": body,
                          "pid": os.getpid(), "home": str(home)}), flush=True)
finally:
    connection.close()
'''


def receive(process):
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        routing.require(selector.select(timeout=15), "native fixture did not respond")
    line = process.stdout.readline()
    routing.require(line, "native fixture exited before responding")
    return json.loads(line)


def digest_existing(root):
    result = {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for directory in (root / ".claude", root / ".codex")
        if directory.exists()
        for path in directory.glob("*.json")
        if path.name != "settings.json"
    }
    if (root / ".claude.json").is_file():
        result[".claude.json"] = hashlib.sha256((root / ".claude.json").read_bytes()).hexdigest()
    return result


def prepare_accounts(swapdex, root, native_bin, env, tool, existing):
    native = native_bin / tool
    native.write_text(f"#!{sys.executable}\n" + FAKE_NATIVE, encoding="utf-8")
    native.chmod(0o755)
    if existing:
        default_home = root / (".claude" if tool == "claude" else ".codex")
        home_key = "CLAUDE_CONFIG_DIR" if tool == "claude" else "CODEX_HOME"
        login_args = ["auth", "login"] if tool == "claude" else ["login", "--device-auth"]
        routing.run([str(native), *login_args], {
            **env, home_key: str(default_home), "SWAPDEX_FIXTURE_ACCOUNT": "original",
        })
        if tool == "claude":
            # The default native home keeps identity beside .claude/, while an
            # explicitly configured account slot keeps it inside that slot.
            (default_home / ".claude.json").rename(root / ".claude.json")
    original = digest_existing(root)
    for name in ("work", "personal"):
        login_args = ["auth", "login"] if tool == "claude" else ["login", "--device-auth"]
        _, stdout, _ = routing.run(
            [swapdex, "run", name, "--tool", tool, "--", *login_args],
            {**env, "SWAPDEX_FIXTURE_ACCOUNT": name},
        )
        event = json.loads(stdout)
        routing.require(event["event"] == "login", "native login was not invoked")
        routing.require(Path(event["home"]).resolve() == routing.slot_dir(root, name, tool),
                        "login was not isolated in the requested account slot")
    routing.require(digest_existing(root) == original, "slot login changed the existing native login")


def detached_proxy_stopped(pid):
    try:
        # Linux adopts the detached proxy below; macOS launchd reaps it.
        if os.waitpid(pid, os.WNOHANG)[0] == pid:
            return True
    except ChildProcessError:
        pass
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    return False


def stop_detached_proxy(root, tool):
    marker = routing.marker_for(root, tool)
    if not marker.exists():
        return
    fields = marker.read_text(encoding="utf-8").split()
    routing.require(len(fields) >= 3 and fields[0].isdigit(), "invalid fixture proxy marker")
    pid = int(fields[0])
    routing.require(pid > 1 and pid != os.getpid(), "invalid fixture proxy pid")
    if detached_proxy_stopped(pid):
        return
    routing.require(os.getpgid(pid) == pid, "fixture proxy did not own its process group")
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pid, sig)
        except ProcessLookupError:
            return
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if detached_proxy_stopped(pid):
                return
            time.sleep(0.05)
    raise AssertionError("detached fixture proxy did not stop")


def verify(swapdex, root, tools, existing, proxy_mode):
    native_bin = root / "native-bin"
    native_bin.mkdir()
    env = routing.safe_env(root, native_bin)
    for tool in tools:
        prepare_accounts(swapdex, root, native_bin, env, tool, existing)
    original = digest_existing(root)
    routing.run([swapdex, "shim"], env)
    shim_dir = routing.store_dir(root) / "bin"
    for tool in ("claude", "codex"):
        routing.require((shim_dir / tool).is_file() == (tool in tools),
                        f"{tool} shim installation did not match available native tools")
    launch_env = {**env, "PATH": f"{shim_dir}:{env['PATH']}"}
    resources = {}
    try:
        for tool in tools:
            routing.run([swapdex, "use", "work", "--tool", tool], launch_env)
            active = routing.store_dir(root) / f"active-{tool}"
            upstream, thread = routing.start_server()
            state = resources[tool] = {
                "upstream": upstream, "thread": thread, "proxy": None, "client": None,
                "active": active, "active_before": active.read_bytes(), "name": "work",
            }
            variable = "SWAPDEX_UPSTREAM" if tool == "claude" else "SWAPDEX_UPSTREAM_CODEX"
            client_env = {**launch_env, variable: f"http://127.0.0.1:{upstream.server_port}"}
            routing.require(not routing.marker_for(root, tool).exists(),
                            "first launch already had a proxy marker")
            if proxy_mode == "foreground":
                state["proxy"], _, _ = routing.start_proxy(
                    swapdex, root, client_env, tool, upstream,
                )
            client = state["client"] = subprocess.Popen(
                [tool], env=client_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, start_new_session=True,
            )
            ready = state["ready"] = receive(client)
            routing.require(ready["event"] == "ready", "plain native command did not start")
            routing.require(Path(ready["home"]).resolve() == routing.slot_dir(root, "work", tool),
                            "plain native launch ignored the selected default")
            state["marker_before"] = routing.marker_for(root, tool).read_bytes()
        for changed in (None, *tools):
            if changed is not None:
                routing.run([swapdex, "serve", "personal", "--tool", changed], launch_env)
                resources[changed]["name"] = "personal"
            for tool, state in resources.items():
                client, ready = state["client"], state["ready"]
                client.stdin.write("turn\n")
                client.stdin.flush()
                event = receive(client)
                routing.require(event["event"] == "turn" and event["status"] == 200
                                and event["body"] == {"ok": True}, "managed turn failed")
                routing.require(event["pid"] == ready["pid"] and event["home"] == ready["home"],
                                "account change restarted the client or moved its conversation home")
                expected = f"Bearer fixture-{tool}-{state['name']}"
                routing.require(state["upstream"].snapshot()[-1]["authorization"] == expected,
                                "managed request used a different account")
        for tool, state in resources.items():
            routing.require(len(state["upstream"].snapshot()) == len(tools) + 1,
                            "unexpected model request count")
            routing.require(state["active"].read_bytes() == state["active_before"],
                            "serve moved the launch default")
            routing.require(routing.marker_for(root, tool).read_bytes() == state["marker_before"],
                            "proxy restarted during a managed conversation")
            client = state["client"]
            client.stdin.write("quit\n")
            client.stdin.flush()
            client.stdin.close()
            routing.require(client.wait(timeout=10) == 0, "native client did not exit cleanly")
        routing.require(digest_existing(root) == original, "native login was changed by slot setup")
    finally:
        cleanup_errors = []
        for tool, state in reversed(list(resources.items())):
            try:
                if state["client"] is not None:
                    routing.stop_process(state["client"])
            except Exception as error:
                cleanup_errors.append(error)
            try:
                if state["proxy"] is not None:
                    routing.stop_process(state["proxy"])
                elif proxy_mode == "autostart":
                    stop_detached_proxy(root, tool)
            except Exception as error:
                cleanup_errors.append(error)
            try:
                routing.stop_server(state["upstream"], state["thread"])
            except Exception as error:
                cleanup_errors.append(error)
        routing.require(not cleanup_errors, f"fixture cleanup failed: {cleanup_errors}")
    for tool in tools:
        routing.require(not routing.marker_for(root, tool).exists(), "fixture proxy marker remained")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--swapdex", required=True, help="absolute native binary path, not an npm wrapper")
    parser.add_argument("--proxy-mode", choices=("autostart", "foreground"), default="autostart",
                        help="autostart tests the shim's first launch on free ports 8787/8788; "
                             "foreground uses allocated fixture ports on machines already running Swapdex")
    args = parser.parse_args()
    binary = Path(args.swapdex)
    routing.require(binary.is_absolute() and binary.is_file() and os.access(binary, os.X_OK),
                    "--swapdex must name a native executable by absolute path")
    if args.proxy_mode == "autostart":
        for port in (8787, 8788):
            with socket.socket() as probe:
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                try:
                    probe.bind(("127.0.0.1", port))
                except OSError as error:
                    raise AssertionError(f"port {port} is in use; use --proxy-mode foreground "
                                         "to verify managed requests without stopping an existing service") from error
        if sys.platform.startswith("linux"):
            import ctypes
            routing.require(ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0,
                            "could not adopt detached fixture proxies for cleanup")
    else:
        print("WARN foreground fixture mode does not test first-launch proxy autostart", flush=True)
    for tools in (("claude",), ("codex",), ("claude", "codex")):
        for existing in (False, True):
            label = "+".join(tools)
            with tempfile.TemporaryDirectory(prefix=f"swapdex-first-use-{label}-") as directory:
                # macOS commonly presents /private/var through /var. Exercise
                # equivalent path spellings on every platform, including Linux.
                real_home = Path(directory) / "actual-home"
                real_home.mkdir()
                alias_home = Path(directory) / "home"
                alias_home.symlink_to(real_home, target_is_directory=True)
                verify(str(binary), alias_home, tools, existing, args.proxy_mode)
            state = "existing native login preserved" if existing else "fresh home"
            print(f"PASS {label} {state} ({args.proxy_mode}): native slot login, shim, "
                  "first turn, next-turn switch", flush=True)


if __name__ == "__main__":
    main()
