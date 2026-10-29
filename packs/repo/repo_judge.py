"""The repository pack's judge: run the repository's checks and score the tests that passed at HEAD.

    python3 /judge/repo_judge.py /judge/repo.json            # {"reward": ..., "checks": {...}}
    python3 /judge/repo_judge.py /judge/repo.json --record   # what passes now: {"reward": 1, "passed": {...}}

repo.json: {"root", "env", "checks": [{"name", "cmd", "junit"?, "timeout"?, "weight"?}], "expected": {name: [ids]}}.
A check with `junit` (a path or glob of JUnit XML files its command writes) scores the fraction of its expected
test cases that passed; a check without scores 1 when its command exits 0. The reward is the checks' weighted
mean. Only the tests that passed at HEAD are expected: a test broken at HEAD says nothing about the task.
"""

import glob
import json
import os
import signal
import subprocess
import sys
import xml.etree.ElementTree as ET

sys.path.insert(0, "/judge")
from judgelib import verdict  # noqa: E402


def run(cmd, cwd, env, timeout):
    """(exit code, output tail); the whole process group is killed on a timeout."""
    p = subprocess.Popen(["/bin/sh", "-c", cmd], cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         start_new_session=True)
    try:
        out, _ = p.communicate(timeout=timeout)
        code = p.returncode
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        out, _ = p.communicate()
        code = 124
    return code, out.decode(errors="replace")[-2000:]


def junit(pattern):
    """{test id: passed} from the JUnit files matching `pattern`; None if there is none or one cannot be read."""
    files = sorted(glob.glob(pattern))
    if not files:
        return None
    out = {}
    for f in files:
        try:
            root = ET.parse(f).getroot()
        except (ET.ParseError, OSError):
            return None
        for case in root.iter("testcase"):
            tid = "%s.%s" % (case.get("classname") or case.get("file") or "", case.get("name") or "")
            ok = not any(child.tag in ("failure", "error", "skipped") for child in case)
            # a test reported twice passes only if it passed every time
            out[tid] = out.get(tid, True) and ok
    return out


def results(cfg, check):
    """{test id: passed} for one check, and its output tail."""
    env = dict(os.environ, **cfg.get("env", {}))
    if check.get("junit"):
        for stale in glob.glob(check["junit"]):
            os.remove(stale)
    code, tail = run(check["cmd"], cfg["root"], env, check.get("timeout", 300))
    if not check.get("junit"):
        return {check["name"]: code == 0}, tail
    return junit(check["junit"]) or {}, tail


def main():
    cfg = json.load(open(sys.argv[1]))
    record = "--record" in sys.argv[2:]
    if record:
        passed, tails = {}, {}
        for check in cfg["checks"]:
            got, tails[check["name"]] = results(cfg, check)
            passed[check["name"]] = sorted(t for t, ok in got.items() if ok)
        verdict(1.0, passed=passed, output=tails)
    scores, details, total = 0.0, {}, 0.0
    for check in cfg["checks"]:
        want = cfg["expected"].get(check["name"]) or []
        if not want:
            continue
        got, tail = results(cfg, check)
        ok = [t for t in want if got.get(t)]
        w = check.get("weight", 1.0)
        scores += w * len(ok) / len(want)
        total += w
        details[check["name"]] = {"passed": len(ok), "expected": len(want),
                                  "failing": [t for t in want if not got.get(t)][:20], "output": tail}
    verdict(scores / total if total else 0.0, checks=details)


if __name__ == "__main__":
    main()
