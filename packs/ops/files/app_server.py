"""A small internal service: reads /etc/app/app.ini and serves /health and /items over HTTP.

[server] port = PORT
[storage] data_dir = DIR (holds items.json)
[auth] token = TOKEN (requests to /items need the header X-Token: TOKEN)
"""

import configparser
import http.server
import json
import os
import sys

cfg = configparser.ConfigParser()
if not cfg.read("/etc/app/app.ini"):
    sys.exit("cannot read /etc/app/app.ini")
port = cfg.getint("server", "port")
data_dir = cfg.get("storage", "data_dir")
token = cfg.get("auth", "token")
with open(os.path.join(data_dir, "items.json")) as f:
    items = json.load(f)


class Handler(http.server.BaseHTTPRequestHandler):
    def reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/health":
            self.reply(200, {"status": "ok"})
        elif self.path == "/items":
            if self.headers.get("X-Token") != token:
                self.reply(401, {"error": "bad token"})
            else:
                self.reply(200, items)
        else:
            self.reply(404, {"error": "not found"})

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
