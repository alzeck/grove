"""Tiny web app for the Grove demo. Calls the API it's wired to."""
import html
import json
import os
import subprocess
import urllib.request
import socketserver
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TITLE = "Grove demo"


def branch():
    try:
        return subprocess.check_output(["git", "rev-parse", "--abbrev-ref", "HEAD"], text=True).strip()
    except Exception:
        return "?"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        try:
            with urllib.request.urlopen(os.environ["API_LOCAL_URL"], timeout=2) as r:
                api = json.loads(r.read())
        except Exception as e:
            api = {"error": str(e)}
        body = f"""<!doctype html><meta charset=utf-8><title>{TITLE}</title>
<body style="font: 16px system-ui; margin: 3rem; line-height: 1.5">
<h1>{TITLE}</h1>
<p>web cluster <b>{html.escape(os.environ.get('GROVE_CLUSTER', '?'))}</b>,
branch <b>{html.escape(branch())}</b>, host <b>{html.escape(self.headers.get('Host', ''))}</b></p>
<p>API (<a href="{os.environ.get('API_URL')}">{os.environ.get('API_URL')}</a>) says:</p>
<pre>{html.escape(json.dumps(api, indent=2))}</pre>
</body>""".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print("web:", fmt % args, flush=True)


class Server(ThreadingHTTPServer):
    # HTTPServer does a reverse DNS lookup here, which can take many seconds.
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = "localhost", self.server_address[1]


port = int(os.environ["PORT"])
print(f"web listening on {port}", flush=True)
Server(("127.0.0.1", port), Handler).serve_forever()
