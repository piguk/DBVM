"""The inventory service: items in /srv/inventory/inventory.db, over HTTP on 127.0.0.1:8080.

GET  /items                -> [{"id", "name", "stock", "reorder_level", "target_level"}, ...]
GET  /items/<id>           -> one item
POST /items/<id>/stock     <- {"set": n} or {"add": n}; -> the item
"""

import http.server
import json
import re
import sqlite3
import threading

DB = "/srv/inventory/inventory.db"
lock = threading.Lock()


def items(where="", args=()):
    db = sqlite3.connect(DB)
    try:
        rows = db.execute("SELECT id, name, stock, reorder_level, target_level FROM items " + where + " ORDER BY id", args).fetchall()
    finally:
        db.close()
    return [dict(zip(("id", "name", "stock", "reorder_level", "target_level"), r)) for r in rows]


class Handler(http.server.BaseHTTPRequestHandler):
    def reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/items":
            return self.reply(200, items())
        m = re.fullmatch(r"/items/(\d+)", self.path)
        found = items("WHERE id = ?", (int(m.group(1)),)) if m else []
        self.reply(200, found[0]) if found else self.reply(404, {"error": "no such item"})

    def do_POST(self):
        m = re.fullmatch(r"/items/(\d+)/stock", self.path)
        if not m:
            return self.reply(404, {"error": "not found"})
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
        except ValueError:
            return self.reply(400, {"error": "the body must be JSON"})
        with lock:
            db = sqlite3.connect(DB)
            try:
                if "set" in body and isinstance(body["set"], int) and body["set"] >= 0:
                    n = db.execute("UPDATE items SET stock = ? WHERE id = ?", (body["set"], int(m.group(1)))).rowcount
                elif "add" in body and isinstance(body["add"], int):
                    n = db.execute("UPDATE items SET stock = MAX(0, stock + ?) WHERE id = ?", (body["add"], int(m.group(1)))).rowcount
                else:
                    return self.reply(400, {"error": "give {\"set\": n} (n >= 0) or {\"add\": n}"})
                db.commit()
            finally:
                db.close()
        if not n:
            return self.reply(404, {"error": "no such item"})
        self.reply(200, items("WHERE id = ?", (int(m.group(1)),))[0])

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", 8080), Handler).serve_forever()
