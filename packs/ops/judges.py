"""Judges of the ops pack (judge files, hidden from the agent).

    python3 /judge/judges.py config|incident|inventory
"""

import json
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, "/judge")
from judgelib import compare_json, verdict  # noqa: E402

spec = json.load(open("/judge/spec.json"))


def config():
    """Start the service with the agent's configuration; check it as its users would."""
    port, token = spec["port"], spec["token"]
    server = subprocess.Popen(["python3", "/opt/app/server.py"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    def get(path, tok=None):
        req = urllib.request.Request("http://127.0.0.1:%d%s" % (port, path), headers={"X-Token": tok} if tok else {})
        try:
            with urllib.request.urlopen(req, timeout=5) as r:
                return r.status, json.load(r)
        except urllib.error.HTTPError as e:
            return e.code, None
        except (urllib.error.URLError, OSError, ValueError):
            return None, None
    try:
        for _ in range(100):
            if server.poll() is not None or get("/health")[0] == 200:
                break
            time.sleep(0.1)
        if server.poll() is not None:
            verdict(0.0, error="the service exited", stderr=server.stderr.read()[-1500:])
        checks = {
            "health": get("/health") == (200, {"status": "ok"}),
            "items": get("/items", token) == (200, spec["items"]),
            "token required": get("/items", "wrong")[0] == 401,
        }
        verdict(sum(checks.values()) / len(checks), checks=checks)
    finally:
        server.kill()


def incident():
    try:
        got = json.load(open("/work/incident.json"))
    except (OSError, ValueError) as e:
        verdict(0.0, error="no JSON answer at /work/incident.json: %s" % e)
    verdict(compare_json(got, spec["expected"], rel=0))


def inventory():
    try:
        db = sqlite3.connect("file:/srv/inventory/inventory.db?mode=ro", uri=True)
        stock = dict(db.execute("SELECT id, stock FROM items").fetchall())
    except sqlite3.Error as e:
        verdict(0.0, error="cannot read the inventory: %s" % e)
    want = {int(k): v for k, v in spec["expected"].items()}
    if set(stock) != set(want):
        verdict(0.0, error="items were added or removed")
    right = [i for i in want if stock[i] == want[i]]
    verdict(len(right) / len(want), wrong=sorted(set(want) - set(right))[:20])


{"config": config, "incident": incident, "inventory": inventory}[sys.argv[1]]()
