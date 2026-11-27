"""Exercise a release build through the hub's SPOP listener using ci/pgo fixtures."""
import argparse
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
from string import Template
import tempfile
import threading
import time

from spop import Client, message


class Upstream(BaseHTTPRequestHandler):
    requests = 0

    def do_GET(self):
        type(self).requests += 1
        body = b'{"keys":[]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def exercise(hub, plugin_dir, fixture, rounds, runner):
    with tempfile.TemporaryDirectory() as work, ThreadingHTTPServer(("127.0.0.1", 0), Upstream) as upstream:
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        sock = Path(work) / "hub.sock"
        config = Path(work) / "hub.toml"
        substitutions = dict(upstream=f"127.0.0.1:{upstream.server_port}", repo=str(Path.cwd()))
        config.write_text(
            f'plugin_dir = {json.dumps(str(plugin_dir.resolve()))}\n'
            f'default_timeout_ms = 10000\nworker_threads = 2\nlog_level = "warn"\n'
            f'[[listeners]]\ntype = "unix"\naddress = "{sock}"\n'
            + Template((fixture / "plugins.toml").read_text()).substitute(substitutions)
        )
        cases = json.loads(Template((fixture / "requests.json").read_text()).substitute(substitutions))
        if not cases:
            raise ValueError("empty training workload")
        with subprocess.Popen([*runner, hub, "--config", config]) as server:
            try:
                deadline = time.monotonic() + 90
                while not sock.exists():
                    if server.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError("hub failed to start")
                    time.sleep(0.05)
                client = Client(sock)
                with closing(client.sock):
                    for _ in range(rounds):
                        for case in cases:
                            args = {k: bytes.fromhex(v["hex"]) if isinstance(v, dict) else v
                                    for k, v in case["args"].items()}
                            result = client.notify(message(case["message"], args))
                            if any(key.endswith(".__status") and value != "ok" for key, value in result.items()):
                                raise ValueError(f"plugin processing failed: {result!r}")
                            for key, value in case["expect"].items():
                                if result.get(key) != value:
                                    raise ValueError(f"{key}: expected {value!r}, got {result!r}")
            finally:
                server.terminate()
                try:
                    server.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    server.kill()
                    raise
            if server.returncode:
                raise RuntimeError(f"hub exited with status {server.returncode}")
        if any(case.get("requires_http") for case in cases) and not Upstream.requests:
            raise RuntimeError("plugin never reached the local HTTP service")
        upstream.shutdown()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hub", type=Path, required=True)
    parser.add_argument("--plugins", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, default=Path("ci/pgo"))
    parser.add_argument("--rounds", type=int, default=512)
    parser.add_argument("runner", nargs="*")
    args = parser.parse_args()
    exercise(args.hub.resolve(), args.plugins, args.fixture, args.rounds, args.runner)
