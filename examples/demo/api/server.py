"""Tiny API for the Grove demo. Reads PORT and GROVE_CLUSTER from the env."""
import json
import os
import subprocess
import socketserver
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GREETING = "hello from the api"


def branch():
    try:
        return subprocess.check_output(["git", "rev-parse", "--abbrev-ref", "HEAD"], text=True).strip()
    except Exception:
        return "?"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            body, kind = b"ok", "text/plain"
        else:
            body = json.dumps({
                "project": "api",
                "cluster": os.environ.get("GROVE_CLUSTER"),
                "branch": branch(),
                "greeting": GREETING,
                "host": self.headers.get("Host"),
            }, indent=2).encode()
            kind = "application/json"
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print("api:", fmt % args, flush=True)


class Server(ThreadingHTTPServer):
    # HTTPServer does a reverse DNS lookup here, which can take many seconds.
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = "localhost", self.server_address[1]


port = int(os.environ["PORT"])
print(f"api listening on {port} (cluster {os.environ.get('GROVE_CLUSTER')})", flush=True)
Server(("127.0.0.1", port), Handler).serve_forever()
