import json
import os
import sqlite3
import subprocess
import sys
import tempfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread


class ModelsEndpoint(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/v1/models":
            self.send_error(403)
            return
        if self.headers.get("Authorization") != "Bearer test-key":
            self.send_error(401)
            return
        body = json.dumps(
            {"data": [{"id": model} for model in ("a", "b", "a", " ", " a")]}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def verify(binary, server, sandbox):
    env = os.environ | {
        "HOME": str(sandbox),
        "CC_SWITCH_CONFIG_DIR": str(sandbox / "cc-switch"),
        "CLAUDE_CONFIG_DIR": str(sandbox / ".claude"),
        "CODEX_HOME": str(sandbox / ".codex"),
        "PI_CODING_AGENT_DIR": str(sandbox / "pi"),
        "XDG_CONFIG_HOME": str(sandbox / ".config"),
        "XDG_RUNTIME_DIR": str(sandbox / ".runtime"),
        "XDG_STATE_HOME": str(sandbox / ".state"),
        "NO_PROXY": "127.0.0.1,localhost",
    }
    base_url = f"http://127.0.0.1:{server.server_port}"

    def add(provider_id, key, *extra):
        return subprocess.run(
            [binary, "--app", "pi", "provider", "add", "--name", provider_id,
             "--id", provider_id, "--base-url", base_url,
             "--api-key", key, *extra],
            env=env, timeout=30, check=False,
        ).returncode

    pi_dir = sandbox / "pi"
    pi_dir.mkdir()
    settings_path = pi_dir / "settings.json"
    defaults = b'{"defaultProvider":"existing","defaultModel":"existing-model"}\n'
    settings_path.write_bytes(defaults)
    models_path = pi_dir / "models.json"
    db_path = sandbox / "cc-switch" / "cc-switch.db"
    assert add("auto", "test-key") == 0
    saved = models_path.read_bytes()
    native_provider = json.loads(saved)["providers"]["auto"]
    assert native_provider["baseUrl"] == f"{base_url}/v1"
    assert native_provider["models"] == [{"id": "a"}, {"id": "b"}, {"id": " a"}]
    with sqlite3.connect(db_path) as connection:
        (stored,) = connection.execute(
            "SELECT settings_config FROM providers WHERE id = 'auto' AND app_type = 'pi'"
        ).fetchone()
    assert json.loads(stored) == native_provider
    assert add("retry", "wrong-key") != 0
    assert models_path.read_bytes() == saved
    assert add("retry", "test-key") == 0
    assert add("manual", "wrong-key", "--model", "manual-model") == 0
    providers = json.loads(models_path.read_bytes())["providers"]
    assert providers["manual"]["models"] == [{"id": "manual-model"}]
    assert providers["retry"]["models"] == native_provider["models"]
    assert settings_path.read_bytes() == defaults


with ThreadingHTTPServer(("127.0.0.1", 0), ModelsEndpoint) as server:
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="pi-model-discovery-") as directory:
            verify(sys.argv[1], server, Path(directory))
    finally:
        server.shutdown()
        thread.join()
