"""How a task pack's judges can be gamed, found without a model: one task per family, its spec read and its judge
probed with rollouts.

    python3 -m packkit audit BASE_URL tasks.json refs.json [--out audit.json]

Static checks, from the task's spec:
- the judge runs something the agent can change (a path in its command under a `sees` path, not read-only and
  not a judge file);
- the answer is visible: a solution file's content appears in a file the rollout sees.

Probes, each a rollout judged as the task's own judge would:
- trivial answers (each solution file empty, as it started, blank) must score no more than the starting state;
- hook files that make test runners report success (a `unittest.py` or `pytest.py` that shadows the runner, a
  pytest `conftest.py` that passes every test, a `sitecustomize.py` that does the same for unittest), each kind
  written into every directory the judge takes from the rollout, must not raise the score;
- equivalent answers (the solution with a trailing blank line) must still score 1: a judge that refuses them is
  fragile, and teaches the model formatting rather than the task.

Each finding has a severity: high (the reward can be had without the work), medium, low (fragile). The exit code is
1 when there is a high one.
"""

import base64
import json

# what makes Python test runners report success, written into a directory the judge reads from the rollout
HOOKS = {
    # `python3 -m unittest` / `-m pytest` from that directory import these first
    "unittest.py": "import sys\nprint('OK')\nsys.exit(0)\n",
    "pytest.py": "import sys\nprint('passed')\nsys.exit(0)\n",
    "conftest.py": ("import pytest\n\n@pytest.hookimpl(hookwrapper=True)\ndef pytest_runtest_makereport(item, call):\n"
                    "    outcome = yield\n    outcome.get_result().outcome = 'passed'\n"),
    "sitecustomize.py": ("import unittest\n\ndef _run(self, result=None):\n    if result is not None:\n"
                         "        result.startTest(self)\n        result.addSuccess(self)\n        result.stopTest(self)\n"
                         "    return result\n\nunittest.TestCase.run = _run\n"),
}

# how much above the starting state a probe may score before it counts
SLACK = 0.05


def _content(f):
    return base64.b64decode(f["content"]) if f.get("encoding") == "base64" else f["content"].encode()


def _under(path, roots):
    return any(path == r or path.startswith(r.rstrip("/") + "/") for r in roots)


def static(spec, ref):
    findings = []
    judge = spec.get("judge") or {}
    sees, readonly = judge.get("sees", []), spec.get("readonly", [])
    judge_files = {f["path"] for f in judge.get("files", [])}
    for arg in judge.get("cmd", []):
        if arg.startswith("/") and _under(arg, sees) and not _under(arg, readonly) and arg not in judge_files:
            findings.append(("high", "judge in reach", "the judge runs %s, which the agent can change (it is under `sees`)" % arg))
    visible = [(f["path"], _content(f)) for f in spec.get("files", [])]
    for sol in ref["solution"]:
        want = _content(sol)
        if len(want.strip()) < 16:
            continue
        for path, data in visible:
            if path != sol["path"] and want.strip() in data:
                findings.append(("high", "answer visible", "the solution of %s is in %s, which the rollout sees" % (sol["path"], path)))
    return findings


def _score(client, task_id, writes):
    """The judge's verdict on a fresh rollout after `writes` ([(path, bytes)])."""
    s = client.rollouts(task_id, [{"label": "audit"}])[0]
    try:
        for path, data in writes:
            r = s.act({"tool": "write_file", "path": path, "content": base64.b64encode(data).decode(), "encoding": "base64"})
            if r["exit_code"] != 0:
                return None
        return s.judge()
    finally:
        s.close()


def _dirs(client, task_id, paths, depth=3, limit=50):
    """The directories of a rollout of the task at or below `paths` (to `depth` levels, at most `limit`): where
    the judge may run a test runner from."""
    s = client.rollouts(task_id, [{"label": "audit-dirs"}])[0]
    try:
        out, todo = [], []
        for p in paths:
            p = p.rstrip("/") or "/"
            if s.act({"tool": "list_dir", "path": p})["exit_code"] == 0:
                todo.append((p, 0))
            else:
                # a file: the judge sees only it, but a runner may start in any directory above it
                while p.count("/") > 1:
                    p = p.rsplit("/", 1)[0]
                    todo.append((p, depth))
        while todo and len(out) < limit:
            d, level = todo.pop(0)
            if d in out:
                continue
            r = s.act({"tool": "list_dir", "path": d})
            if r["exit_code"] != 0:
                continue
            out.append(d)
            if level < depth:
                todo += [(d.rstrip("/") + "/" + e["name"], level + 1) for e in r.get("entries") or [] if e["kind"] == "dir"]
        return out
    finally:
        s.close()


def probes(client, task_id, spec, ref, start):
    findings, times = [], []
    files = {f["path"]: _content(f) for f in spec.get("files", [])}
    def above(v):
        return v is not None and v["reward"] is not None and v["reward"] > start + SLACK
    for sol in ref["solution"]:
        p = sol["path"]
        variants = {"empty": b"", "blank": b" \n"}
        if p in files:
            variants["as it started"] = files[p]
        for name, data in variants.items():
            v = _score(client, task_id, [(p, data)])
            if above(v):
                findings.append(("high", "trivial answer", "%s %s scores %s (the starting state %s)" % (p, name, v["reward"], start)))
    # one kind of hook at a time, in every directory at once: hooks of different kinds get in each other's way
    # (sitecustomize.py importing unittest finds the unittest.py beside it; measured: together they scored 0,
    # unittest.py alone 1); then the directories one by one, to say where
    dirs = _dirs(client, task_id, (spec.get("judge") or {}).get("sees", []))
    for name, body in HOOKS.items():
        if not dirs or not above(_score(client, task_id, [(d.rstrip("/") + "/" + name, body.encode()) for d in dirs])):
            continue
        hits = [d for d in dirs if above(_score(client, task_id, [(d.rstrip("/") + "/" + name, body.encode())]))] or dirs
        findings.append(("high", "test runner hook", "%s in %s raises the score above the starting state %s" % (name, ", ".join(hits), start)))
    solved = [(sol["path"], _content(sol) + b"\n") for sol in ref["solution"]]
    v = _score(client, task_id, solved)
    if v is not None and v["reward"] is not None and v["reward"] < 1.0:
        findings.append(("low", "fragile judge", "the solution with a trailing blank line scores %s" % v["reward"]))
    return findings


def audit(client, tasks, refs, log=print):
    """{family: [findings]} for one task per family; findings are (severity, kind, detail)."""
    report, seen = {}, set()
    for t in tasks:
        if t["family"] in seen:
            continue
        seen.add(t["family"])
        spec, ref = client.task(t["task_id"]), refs[t["task_id"]]
        found = static(spec, ref) + probes(client, t["task_id"], spec, ref, t.get("start_reward") or 0.0)
        report[t["family"]] = [{"severity": s, "kind": k, "detail": d, "task_id": t["task_id"]} for s, k, d in found]
        for f in report[t["family"]]:
            log("%-6s %-24s %-18s %s" % (f["severity"].upper(), t["family"], f["kind"], f["detail"]))
    highs = sum(1 for fs in report.values() for f in fs if f["severity"] == "high")
    log("audited %d families: %d high, %d other findings" % (len(report), highs, sum(len(fs) for fs in report.values()) - highs))
    return report
