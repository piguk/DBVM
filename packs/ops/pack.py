"""The ops pack for packkit: an incident in a system's logs, a service's broken configuration, and a stateful
service to operate through its API. See README.md."""

import base64
import datetime
import itertools
import json
import os
import random
import sqlite3
import tempfile

import packkit

HERE = os.path.dirname(os.path.abspath(__file__))


def read(*p):
    with open(os.path.join(HERE, *p)) as f:
        return f.read()


def judge(kind, spec, sees, services=False):
    return {"cmd": ["python3", "/judge/judges.py", kind], "sees": sees, "timeout_secs": 60, "services": services,
            "files": [{"path": "/judge/judges.py", "content": read("judges.py")},
                      {"path": "/judge/judgelib.py", "content": packkit.judgelib()},
                      {"path": "/judge/spec.json", "content": json.dumps(spec)}]}


# ---- an incident in the logs ----

SERVICES = ["api", "auth", "billing", "cache", "db", "queue"]
# who calls whom: an outage spreads to the callers
CALLERS = {"db": ["auth", "billing", "queue"], "cache": ["api"], "auth": ["api"], "queue": ["billing"], "billing": ["api"]}
CODES = {"db": ["E-DB-CONN-REFUSED", "E-DB-DISK-FULL", "E-DB-LOCK-TIMEOUT"], "cache": ["E-CACHE-EVICT-STORM", "E-CACHE-OOM"],
         "auth": ["E-AUTH-KEY-EXPIRED", "E-AUTH-CLOCK-SKEW"], "queue": ["E-QUEUE-BACKLOG", "E-QUEUE-PARTITION"]}


def incident(rng):
    start = datetime.datetime(2026, 3, 1, 10, 0, 0)
    root = rng.choice(sorted(CODES))
    code = rng.choice(CODES[root])
    t0 = start + datetime.timedelta(seconds=rng.randrange(900, 2700), milliseconds=rng.randrange(1000))
    # when each service starts failing: the root first, then its callers, theirs after them
    failing = {root: t0}
    frontier = [root]
    while frontier:
        s = frontier.pop()
        for c in CALLERS.get(s, []):
            t = failing[s] + datetime.timedelta(seconds=rng.randrange(3, 40), milliseconds=rng.randrange(1000))
            if c not in failing or t < failing[c]:
                failing[c] = t
                frontier.append(c)
    lines = {s: [] for s in SERVICES}
    for s in SERVICES:
        t = start
        while t < start + datetime.timedelta(hours=1):
            t += datetime.timedelta(milliseconds=rng.randrange(500, 9000))
            if s in failing and t >= failing[s]:
                if s == root:
                    msg = "ERROR %s code=%s %s" % (s, code, rng.choice(["request failed", "operation aborted", "retrying"]))
                else:
                    msg = "ERROR %s code=E-UPSTREAM-5XX upstream call failed" % s
            elif rng.random() < 0.08:
                # warnings with codes of their own: not the incident
                msg = "WARN %s code=W-SLOW-%d slow request %dms" % (s, rng.randrange(1, 4), rng.randrange(800, 3000))
            else:
                msg = "INFO %s request ok %dms" % (s, rng.randrange(2, 300))
            lines[s].append("%s %s" % (t.isoformat(timespec="milliseconds"), msg))
    files = [{"path": "/var/log/app/%s.log" % s, "content": "\n".join(ls) + "\n"} for s, ls in lines.items()]
    expected = {"service": root, "error": code, "first_seen": t0.strftime("%H:%M:%S")}
    text = ("An outage hit the system whose service logs are in /var/log/app (read-only); the services call each other, so "
            "errors spread from where the outage began. Find where it began and write /work/incident.json: "
            '{"service": the service where the errors began, "error": the error code it logged, "first_seen": the time '
            "(HH:MM:SS) of that service's first error}.")
    spec = {"instruction": text, "readonly": ["/var/log/app"], "files": files,
            "judge": judge("incident", {"expected": expected}, ["/work"])}
    cheats = [("an empty object", [{"path": "/work/incident.json", "content": "{}"}]),
              ("not JSON", [{"path": "/work/incident.json", "content": "service=db"}])]
    return packkit.Candidate("logs/incident", spec, [{"path": "/work/incident.json", "content": json.dumps(expected)}],
                             {"root": root, "code": code}, cheats)


# ---- a service's broken configuration ----

def config(rng):
    port, token = rng.randrange(8100, 8999), "%032x" % rng.getrandbits(128)
    items = [{"id": i, "name": "part-%03d" % rng.randrange(1000)} for i in range(1, rng.randrange(3, 8))]
    good = "[server]\nport = %d\n\n[storage]\ndata_dir = /srv/app/data\n\n[auth]\ntoken = %s\n" % (port, token)
    faults = {
        "a typo in the port": lambda c: c.replace("port = %d" % port, "port = %s" % str(port).replace("0", "O", 1) if "0" in str(port) else "port = %dx" % port),
        "the wrong data directory": lambda c: c.replace("/srv/app/data", "/srv/app/dat"),
        "a renamed section": lambda c: c.replace("[auth]", "[authentication]"),
        "another port": lambda c: c.replace("port = %d" % port, "port = %d" % (port + 1)),
    }
    chosen = rng.sample(sorted(faults), rng.choice([1, 2]))
    broken = good
    for f in chosen:
        broken = faults[f](broken)
    text = ("The internal service /opt/app/server.py (read-only) must serve on port %d, with the items in /srv/app/data, "
            "to callers that present the token in /etc/app/app.ini. It reads its configuration from /etc/app/app.ini and "
            "does not work. Fix the configuration (only /etc/app and /srv/app may change); you can run the service to "
            "check it." % port)
    files = [{"path": "/opt/app/server.py", "content": read("files", "app_server.py")},
             {"path": "/etc/app/app.ini", "content": broken},
             {"path": "/srv/app/data/items.json", "content": json.dumps(items)}]
    spec = {"instruction": text, "readonly": ["/opt/app"], "files": files,
            "judge": judge("config", {"port": port, "token": token, "items": items}, ["/etc/app", "/srv/app"])}
    cheats = [("an empty configuration", [{"path": "/etc/app/app.ini", "content": ""}]),
              ("the service on another port", [{"path": "/etc/app/app.ini", "content": faults["another port"](good)}])]
    return packkit.Candidate("config/repair", spec, [{"path": "/etc/app/app.ini", "content": good}], {"faults": chosen}, cheats)


# ---- a stateful service operated through its API ----

def inventory_db(rows):
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    try:
        db = sqlite3.connect(path)
        db.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL, stock INTEGER NOT NULL, "
                   "reorder_level INTEGER NOT NULL, target_level INTEGER NOT NULL)")
        db.executemany("INSERT INTO items VALUES (?,?,?,?,?)", rows)
        db.commit()
        db.close()
        with open(path, "rb") as f:
            return base64.b64encode(f.read()).decode()
    finally:
        os.remove(path)


def inventory(rng):
    rows = []
    for i in range(1, rng.randrange(20, 41)):
        reorder = rng.randrange(5, 30)
        target = reorder + rng.randrange(10, 60)
        stock = rng.randrange(0, reorder) if rng.random() < 0.35 else rng.randrange(reorder, target + 20)
        rows.append((i, "sku-%04d" % rng.randrange(10000), stock, reorder, target))
    if not any(r[2] < r[3] for r in rows):
        rows[0] = (rows[0][0], rows[0][1], 0, rows[0][3], rows[0][4])
    expected = {r[0]: (r[4] if r[2] < r[3] else r[2]) for r in rows}
    text = ("The inventory service runs at http://127.0.0.1:8080: GET /items lists the items (id, name, stock, reorder_level, "
            "target_level); POST /items/<id>/stock with {\"set\": n} or {\"add\": n} changes an item's stock. Restock every "
            "item whose stock is below its reorder level to exactly its target level. Leave the other items as they are.")
    ready = "python3 -c \"import urllib.request; urllib.request.urlopen('http://127.0.0.1:8080/items', timeout=2)\""
    spec = {"instruction": text, "readonly": ["/opt/inventory"],
            "files": [{"path": "/opt/inventory/server.py", "content": read("files", "inventory_server.py")},
                      {"path": "/srv/inventory/inventory.db", "content": inventory_db(rows), "encoding": "base64"}],
            "services": [{"name": "inventory", "run": "exec python3 /opt/inventory/server.py", "ready": ready}],
            "judge": judge("inventory", {"expected": expected}, ["/srv/inventory"])}
    solved = [(r[0], r[1], expected[r[0]], r[3], r[4]) for r in rows]
    emptied = [(r[0], r[1], 0, r[3], r[4]) for r in rows]
    db = "/srv/inventory/inventory.db"
    cheats = [("every stock set to zero", [{"path": db, "content": inventory_db(emptied), "encoding": "base64"}]),
              ("the database replaced", [{"path": db, "content": "not a database"}])]
    return packkit.Candidate("service/inventory", spec, [{"path": db, "content": inventory_db(solved), "encoding": "base64"}],
                             {"items": len(rows), "to_restock": sum(1 for r in rows if r[2] < r[3])}, cheats)


FAMILIES = [incident, config, inventory]


def endless(make, rng):
    while True:
        yield make(rng)


def candidates(rng):
    # a generator function per family: a generator expression in the loop would call the last family's `make`
    out = {}
    for make in FAMILIES:
        r = random.Random(rng.random())
        first = make(r)
        out[first.family] = itertools.chain([first], endless(make, r))
    return out
