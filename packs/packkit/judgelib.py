"""Helpers for judges (standard library only). Ship it as a judge file next to the judge:

    {"path": "/judge/judgelib.py", "content": packkit.judgelib()}

and in the judge: ``sys.path.insert(0, "/judge"); from judgelib import *``.
"""

import csv
import json
import math
import subprocess
import sys


def verdict(reward, **details):
    """Print the judge's answer (one JSON object with `reward`) and exit."""
    print(json.dumps(dict(reward=max(0.0, min(1.0, float(reward))), **details)))
    sys.exit(0)


def run(cmd, timeout=120, **kw):
    """Run `cmd` (a list); (exit code, stdout, stderr), exit code 124 on a timeout."""
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kw)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return 124, "", "timed out after %ss" % timeout


def near(a, b, rel=1e-6, abs_=1e-9):
    """Whether two values are equal: numbers within tolerance, anything else exactly."""
    x, y = number(a), number(b)
    if x is not None and y is not None:
        return math.isclose(x, y, rel_tol=rel, abs_tol=abs_)
    return str(a).strip() == str(b).strip()


def number(v):
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        return float(v)
    try:
        return float(str(v).strip())
    except ValueError:
        return None


def score_relative(got, target, tol, zero_at):
    """1 within `tol` (relative) of `target`, falling to 0 at `zero_at` times (or a `zero_at`-th of) it."""
    if got is None or got == 0 or (got > 0) != (target > 0):
        return 0.0
    err = abs(math.log(got / target))
    ok, bad = math.log(1 + tol), math.log(zero_at)
    return 1.0 if err <= ok else max(0.0, 1 - (err - ok) / (bad - ok))


def read_csv(path):
    """(header, rows) of a CSV file, cells stripped; (None, []) if it cannot be read."""
    try:
        with open(path, newline="") as f:
            rows = [[c.strip() for c in r] for r in csv.reader(f) if any(c.strip() for c in r)]
    except (OSError, UnicodeDecodeError, csv.Error):
        return None, []
    return (rows[0], rows[1:]) if rows else (None, [])


def compare_tables(got, want, ordered=False, rel=1e-6):
    """The share of `want`'s rows that `got` has (each matched once; cells compared with `near`), less for
    extra rows: matched / max(len(got), len(want)). With `ordered`, row i must match row i."""
    if not want:
        return 1.0 if not got else 0.0
    def same(r, s):
        return len(r) == len(s) and all(near(a, b, rel) for a, b in zip(r, s))
    if ordered:
        matched = sum(1 for r, s in zip(got, want) if same(r, s))
    else:
        left, matched = list(got), 0
        for s in want:
            for i, r in enumerate(left):
                if same(r, s):
                    matched += 1
                    del left[i]
                    break
    return matched / max(len(got), len(want))


def compare_json(got, want, rel=1e-6):
    """The share of `want`'s leaves (dict keys, list items in order, values) that `got` has, compared with `near`."""
    def leaves(w, g):
        if isinstance(w, dict):
            out = []
            for k, v in w.items():
                out += leaves(v, g.get(k) if isinstance(g, dict) else None)
            return out
        if isinstance(w, list):
            out = []
            for i, v in enumerate(w):
                out += leaves(v, g[i] if isinstance(g, list) and i < len(g) else None)
            return out
        return [g is not None and near(g, w, rel)]
    ls = leaves(want, got)
    return sum(ls) / len(ls) if ls else 1.0
