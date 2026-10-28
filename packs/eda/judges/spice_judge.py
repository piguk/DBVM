"""Judge of an analog task: checks the agent's subcircuit, simulates it with the read-only measurement deck, and
scores the measured metrics against the task's specification.

    python3 /tests/judge.py

Reads /work/circuit.cir (one subcircuit ``dut``), /tests/spec.json and /tests/measure.cir. The subcircuit may
hold only the element kinds the spec allows (no sources: a source could hold the output at the target), plain
positive values within the spec's ranges, and no parameters, expressions or directives. ``reward`` is the
lowest metric score (every metric must be met); a metric scores 1 within its tolerance and falls to 0 at
``zero_at`` times (or a ``zero_at``-th of) its target.
"""

import json
import math
import re
import shutil
import subprocess
import sys

SUFFIX = {"f": 1e-15, "p": 1e-12, "n": 1e-9, "u": 1e-6, "m": 1e-3, "k": 1e3, "meg": 1e6, "g": 1e9, "t": 1e12}


def value(text):
    m = re.fullmatch(r"([0-9]*\.?[0-9]+(?:e[-+]?[0-9]+)?)(meg|[fpnumkgt])?[a-z]*", text.lower())
    if not m:
        return None
    return float(m.group(1)) * SUFFIX.get(m.group(2) or "", 1.0)


def verdict(reward, **details):
    print(json.dumps(dict(reward=reward, **details)))
    sys.exit(0)


def check(lines, spec):
    """The subcircuit's elements as (kind, value), or an error."""
    body = [l.split(";")[0].strip() for l in lines]
    body = [l for l in body if l and not l.startswith("*")]
    if not body or body[0].lower().split() != [".subckt", "dut"] + spec["ports"]:
        return None, "the file must start with: .subckt dut %s" % " ".join(spec["ports"])
    if body[-1].lower() != ".ends":
        return None, "the file must end with .ends"
    elements = []
    for l in body[1:-1]:
        if any(c in l for c in "{}='\""):
            return None, "parameters and expressions are not allowed: %s" % l
        f = l.split()
        kind = f[0][0].upper()
        if kind not in spec["allow"]:
            return None, "element kind %s is not allowed (allowed: %s): %s" % (kind, ", ".join(spec["allow"]), l)
        if kind == "X":
            if len(f) < 3 or f[-1].lower() not in spec.get("subckts", []):
                return None, "only these subcircuits may be used: %s" % spec.get("subckts", [])
            continue
        if len(f) != 4:
            return None, "an element is: name node node value: %s" % l
        v = value(f[3])
        if v is None or not v > 0 or not math.isfinite(v):
            return None, "not a positive value: %s" % l
        lo, hi = spec["ranges"][kind]
        if not lo <= v <= hi:
            return None, "%s outside %g..%g: %s" % (kind, lo, hi, l)
        elements.append((kind, v))
    if len(elements) + sum(1 for l in body[1:-1] if l[0].upper() == "X") > spec.get("max_elements", 8):
        return None, "at most %d elements" % spec.get("max_elements", 8)
    return elements, None


def score(metric, got):
    if "min" in metric:
        return 1.0 if got >= metric["min"] else 0.0
    if "max" in metric:
        return 1.0 if got <= metric["max"] else 0.0
    target, tol, zero_at = metric["target"], metric["tol"], metric["zero_at"]
    if got == 0 or (got > 0) != (target > 0):
        return 0.0
    err = abs(math.log(got / target))
    ok, bad = math.log(1 + tol), math.log(zero_at)
    return 1.0 if err <= ok else max(0.0, 1 - (err - ok) / (bad - ok))


def main():
    spec = json.load(open("/tests/spec.json"))
    try:
        lines = open("/work/circuit.cir").read().splitlines()
    except OSError as e:
        verdict(0.0, error="cannot read the circuit: %s" % e)
    _, why = check(lines, spec)
    if why:
        verdict(0.0, error=why)
    work = "/tmp/judge-spice"
    shutil.rmtree(work, ignore_errors=True)
    shutil.copytree("/tests", work)
    try:
        r = subprocess.run(["ngspice", "-b", "measure.cir"], cwd=work, capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        verdict(0.0, error="simulation timed out")
    out = r.stdout + r.stderr
    measured = {}
    for name, v in re.findall(r"^\s*METRIC\s+(\w+)\s+([-+0-9.eE]+)\s*$", out, flags=re.M):
        measured[name] = float(v)
    scores = {}
    for metric in spec["metrics"]:
        if metric["name"] not in measured:
            verdict(0.0, error="the simulation measured no %s" % metric["name"], output=out[-1500:])
        scores[metric["name"]] = score(metric, measured[metric["name"]])
    verdict(min(scores.values()), measured=measured, scores=scores)


if __name__ == "__main__":
    main()
