#!/usr/bin/env python3
"""Verify stock Codex profile routing with fake accounts and loopback endpoints.

Usage: python3 scripts/verify-codex-profile-routing.py --codex /path/to/codex \
    --swapdex /path/to/swapdex
No live credentials, model endpoints, or running user sessions are used.
"""

if not __debug__:
    raise SystemExit("verification requires assertions; rerun Python without -O")

import argparse
import base64
import contextlib
import datetime
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def load_support():
    path = Path(__file__).with_name("verify-codex-session-resume.py")
    spec = importlib.util.spec_from_file_location("codex_resume_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


support = load_support()


def credential(account):
    def part(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).rstrip(b"=").decode()

    token = part({"alg": "none"}) + "." + part({
        "exp": 4102444800,
        "sub": f"fixture-user-{account}",
        "email": f"{account}@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": f"fixture-account-{account}",
            "chatgpt_plan_type": "plus",
        },
    }) + ".fixture"
    return {
        "auth_mode": "chatgpt",
        "tokens": {"id_token": token, "access_token": token,
                   "refresh_token": f"fixture-refresh-{account}",
                   "account_id": f"fixture-account-{account}"},
        "last_refresh": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    }


def model_requests(server):
    return [row for row in server.snapshot()
            if row[0] == "POST" and row[1].split("?", 1)[0].endswith("/responses")]


def verify(codex, swapdex, root):
    home = root / ".codex"
    selected_home = root / "selected-home"
    workspace = root / "workspace"
    native_bin = root / "native-bin"
    temporary = root / "tmp"
    store = root / ".local/share/swapdex"
    for directory in (home, selected_home, workspace, native_bin, temporary, store):
        directory.mkdir(parents=True, exist_ok=True)
    (native_bin / "codex").symlink_to(codex)
    credentials = {name: credential(name) for name in ("launch", "selected")}
    for name, directory in (("launch", home), ("selected", selected_home)):
        (directory / "auth.json").write_text(json.dumps(credentials[name]))
    original_auth = (home / "auth.json").read_bytes()
    (store / "slots.json").write_text(json.dumps([
        {"name": "launch", "id": "aaaa1111", "tool": "codex", "config_dir": str(home)},
        {"name": "selected", "id": "bbbb2222", "tool": "codex",
         "config_dir": str(selected_home)},
    ]))
    (store / "active-codex").write_text(str(home))
    (store / "serving-codex").write_text(str(selected_home))
    env = {
        "PATH": f"{native_bin}:/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(root), "CODEX_HOME": str(home), "SWAPDEX_ROOT": str(root),
        "XDG_CACHE_HOME": str(root / ".cache"),
        "XDG_CONFIG_HOME": str(root / ".config"),
        "XDG_DATA_HOME": str(root / ".local/share"),
        "XDG_STATE_HOME": str(root / ".local/state"),
        "TMPDIR": str(temporary), "SHELL": "/bin/sh", "USER": "fixture",
        "LOGNAME": "fixture", "LANG": "C", "LC_ALL": "C", "TERM": "dumb",
        "NO_COLOR": "1", "NO_PROXY": "localhost,127.0.0.1",
        "no_proxy": "localhost,127.0.0.1", "SWAPDEX_CURL": "/usr/bin/false",
        "SWAPDEX_OAUTH_URL": "http://127.0.0.1:1/oauth/token",
        "SWAPDEX_CODEX_OAUTH_URL": "http://127.0.0.1:1/oauth/token",
        "SWAPDEX_FIXTURE_KEY": "fixture-custom-key",
    }

    with contextlib.ExitStack() as cleanup:
        upstream, thread = support.start_http_server(support.Responses)
        cleanup.callback(support.stop_http_server, upstream, thread)
        custom, thread = support.start_http_server(support.Responses)
        cleanup.callback(support.stop_http_server, custom, thread)
        direct, thread = support.start_http_server(support.RejectDirect)
        cleanup.callback(support.stop_http_server, direct, thread)
        env["SWAPDEX_UPSTREAM_CODEX"] = f"http://127.0.0.1:{upstream.server_port}"
        (home / "config.toml").write_text(
            'model="gpt-6"\ncli_auth_credentials_store="file"\n'
            f'openai_base_url="http://127.0.0.1:{direct.server_port}/v1"\n'
            f'chatgpt_base_url="http://127.0.0.1:{direct.server_port}"\n'
            '[model_providers.fixture]\nname="Fixture"\nwire_api="responses"\n'
            f'base_url="http://127.0.0.1:{custom.server_port}/v1"\n'
            'env_key="SWAPDEX_FIXTURE_KEY"\nrequires_openai_auth=false\n'
            f'[projects.{json.dumps(str(workspace))}]\ntrust_level="trusted"\n'
        )
        (home / "worker.config.toml").write_text('model_reasoning_effort="low"\n')
        (home / "custom.config.toml").write_text('model_provider="fixture"\n')
        proxy = subprocess.Popen(
            [swapdex, "proxy", "--port", "0", "--tool", "codex"], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
        )
        cleanup.callback(support.stop, proxy)
        marker = store / "proxy-codex"
        deadline = time.monotonic() + 10
        while not marker.exists():
            assert proxy.poll() is None and time.monotonic() < deadline, "proxy startup failed"
            time.sleep(0.05)
        support.run([swapdex, "shim"], env)
        shim = store / "bin/codex"
        assert shim.is_file(), "generated Codex shim missing"

        def turn(profile):
            output = support.run([
                str(shim), "--strict-config", "exec", "--skip-git-repo-check",
                "--json", "-C", str(workspace), *profile, "Return fixture-ok.",
            ], env)
            events = [json.loads(line) for line in output.splitlines()
                      if line.startswith(b"{")]
            assert any(event.get("type") == "turn.completed" for event in events), events
            assert any(event.get("item", {}).get("text") == "fixture-ok"
                       for event in events), events

        for profile in (["-p", "worker"], ["-pworker"],
                        ["--profile", "worker"], ["--profile=worker"]):
            before = len(model_requests(upstream))
            turn(profile)
            requests = model_requests(upstream)
            assert len(requests) == before + 1, (profile, requests)
            _method, _path, upgrade, authorization, account = requests[-1]
            assert upgrade is None
            assert authorization == "Bearer " + credentials["selected"]["tokens"]["access_token"]
            assert account == "fixture-account-selected", (profile, account)
            assert not model_requests(custom), "managed profile used the custom provider"
            assert not any(path.split("?", 1)[0].endswith(("/models", "/responses"))
                           for _method, path in direct.snapshot()), direct.snapshot()
            assert (home / "auth.json").read_bytes() == original_auth
            print(f"PASS stock Codex {' '.join(profile)} uses selected account; launch auth unchanged",
                  flush=True)

        managed_count = len(model_requests(upstream))
        turn(["-p", "custom"])
        requests = model_requests(custom)
        assert len(requests) == 1, requests
        assert requests[0][3] == "Bearer fixture-custom-key", requests
        assert requests[0][4] is None, requests
        assert len(model_requests(upstream)) == managed_count
        assert (home / "auth.json").read_bytes() == original_auth
        assert (store / "active-codex").read_text() == str(home)
        assert proxy.poll() is None
        print("PASS custom profile preserves its provider and API key", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--swapdex", required=True)
    arguments = parser.parse_args()
    codex = Path(arguments.codex).resolve()
    swapdex = Path(arguments.swapdex).resolve()
    for executable in (codex, swapdex):
        assert executable.is_file() and os.access(executable, os.X_OK), executable
    with tempfile.TemporaryDirectory(prefix="swapdex-native-profile-") as directory:
        verify(str(codex), str(swapdex), Path(directory))


if __name__ == "__main__":
    main()
