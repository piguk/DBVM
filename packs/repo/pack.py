"""The repository pack for packkit: tasks made from a repository's own history and tests. See README.md.

The configuration is a JSON file named by REPO_PACK_CONFIG:

    {"name": "calc", "checkout": "../calc", "root": "/work/repo",
     "include": ["src/**/*.py"], "exclude": [], "hidden": ["tests/hidden/**"],
     "checks": [{"name": "unit", "cmd": "python3 /judge/unittest_junit.py -s tests -t . -o /tmp/unit.xml",
                 "junit": "/tmp/unit.xml", "agent_cmd": "python3 -m unittest discover -s tests -t ."}],
     "modes": ["mutation", "history"], "history": {"commits": 200}}
"""

import base64
import difflib
import hashlib
import json
import os
import re
import subprocess

import packkit
from packkit.mutate import mutations

HERE = os.path.dirname(os.path.abspath(__file__))
LANGS = {".py": "python", ".c": "c", ".h": "c", ".cc": "c", ".cpp": "c", ".hpp": "c", ".java": "c", ".js": "c",
         ".ts": "c", ".go": "c", ".rs": "c", ".cs": "c", ".kt": "c", ".scala": "c", ".swift": "c"}

# what `prepare` learned from the template: the tests that pass at HEAD
STATE = {}


def config():
    path = os.environ.get("REPO_PACK_CONFIG")
    if not path:
        raise SystemExit("repo pack: set REPO_PACK_CONFIG to the pack's configuration (a JSON file)")
    with open(path) as f:
        cfg = json.load(f)
    cfg["checkout"] = os.path.join(os.path.dirname(os.path.abspath(path)), cfg["checkout"])
    cfg["root"] = cfg["root"].rstrip("/")
    for k, v in (("include", ["**"]), ("exclude", []), ("hidden", []), ("modes", ["mutation", "history"]),
                 ("env", {}), ("limits", {}), ("history", {}), ("timeout", 300), ("instruction", "")):
        cfg.setdefault(k, v)
    for c in cfg["checks"]:
        c.setdefault("timeout", cfg["timeout"])
    return cfg


def git(cfg, *args, check=True):
    r = subprocess.run(["git", "-C", cfg["checkout"]] + list(args), capture_output=True)
    if check and r.returncode != 0:
        raise RuntimeError("git %s: %s" % (" ".join(args), r.stderr.decode(errors="replace").strip()))
    return r


def _glob(g):
    """A path glob as a regex: `*` and `?` within a directory, `**` across directories, `**/` also none."""
    out, i = "", 0
    while i < len(g):
        if g.startswith("**/", i):
            out, i = out + "(?:.*/)?", i + 3
        elif g.startswith("**", i):
            out, i = out + ".*", i + 2
        elif g[i] == "*":
            out, i = out + "[^/]*", i + 1
        elif g[i] == "?":
            out, i = out + "[^/]", i + 1
        else:
            out, i = out + re.escape(g[i]), i + 1
    return re.compile(out + r"\Z")


def _match(path, globs):
    return any(_glob(g).match(path) for g in globs)


def layout(cfg, rev="HEAD"):
    """(source files, hidden files) of the repository at `rev`, paths relative to it."""
    files = git(cfg, "ls-tree", "-r", "--name-only", "-z", rev).stdout.decode().split("\0")
    files = [f for f in files if f]
    hidden = [f for f in files if _match(f, cfg["hidden"])]
    sources = [f for f in files if _match(f, cfg["include"]) and not _match(f, cfg["exclude"]) and f not in hidden]
    return sources, hidden


def blob(cfg, rev, path):
    r = git(cfg, "show", "%s:%s" % (rev, path), check=False)
    return r.stdout if r.returncode == 0 else None


def tfile(path, data):
    try:
        return {"path": path, "content": data.decode()}
    except UnicodeDecodeError:
        return {"path": path, "content": base64.b64encode(data).decode(), "encoding": "base64"}


def judge(cfg):
    conf = {"root": cfg["root"], "env": cfg["env"], "expected": STATE.get("expected", {}),
            "checks": [{k: c[k] for k in ("name", "cmd", "junit", "timeout", "weight") if k in c} for c in cfg["checks"]]}
    files = [{"path": "/judge/repo_judge.py", "content": open(os.path.join(HERE, "repo_judge.py")).read()},
             {"path": "/judge/unittest_junit.py", "content": open(os.path.join(HERE, "unittest_junit.py")).read()},
             {"path": "/judge/judgelib.py", "content": packkit.judgelib()},
             {"path": "/judge/repo.json", "content": json.dumps(conf)}]
    files += [tfile("%s/%s" % (cfg["root"], h), blob(cfg, STATE["head"], h)) for h in STATE.get("hidden", [])]
    return {"cmd": ["python3", "/judge/repo_judge.py", "/judge/repo.json"],
            # only the source files come from the rollout: the tests, their configuration and anything the agent
            # added (a conftest.py, a module that shadows the test runner) are the template's
            "sees": ["%s/%s" % (cfg["root"], s) for s in STATE["sources"]],
            "timeout_secs": sum(c["timeout"] for c in cfg["checks"]) + 60, "files": files}


# what the rollout runs to compare the template's repository with the checkout: (path, sha256) of each file
PROBE = """
import hashlib, json, os, sys
want = json.load(open(sys.argv[1]))
root = want["root"]
def digest(p):
    try:
        return hashlib.sha256(open(os.path.join(root, p), "rb").read()).hexdigest()
    except OSError:
        return None
print(json.dumps({"differ": [p for p, d in want["sources"].items() if digest(p) != d][:20],
                  "hidden_present": [p for p in want["hidden"] if os.path.lexists(os.path.join(root, p))][:20]}))
"""


def prepare(client, template, log=print):
    """Check that the template holds the checkout's HEAD (without the hidden files) and record which tests pass
    there: those are the ones a task expects to pass."""
    cfg = config()
    head = git(cfg, "rev-parse", "HEAD").stdout.decode().strip()
    sources, hidden = layout(cfg)
    STATE.update(head=head, sources=sources, hidden=hidden)
    spec = {"template_id": template, "judge": dict(judge(cfg), cmd=["python3", "/judge/repo_judge.py", "/judge/repo.json", "--record"])}
    task = client.create_task(spec)
    try:
        s = client.rollouts(task, [{"label": "prepare"}])[0]
        try:
            want = {"root": cfg["root"], "hidden": hidden,
                    "sources": {p: hashlib.sha256(blob(cfg, head, p)).hexdigest() for p in sources}}
            for path, data in (("/tmp/repo-probe.py", PROBE), ("/tmp/repo-want.json", json.dumps(want))):
                if s.write_file(path, data)["exit_code"] != 0:
                    raise RuntimeError("repo pack: cannot write %s in a rollout" % path)
            r = s.bash("python3 /tmp/repo-probe.py /tmp/repo-want.json")
            if r["exit_code"] != 0:
                raise RuntimeError("repo pack: the probe failed (python3 in the template?): %s" % r["stderr"])
            got = json.loads(r["output"])
            if got["differ"]:
                raise RuntimeError("repo pack: the template's %s is not %s at %s: %s differ" % (cfg["root"], cfg["checkout"], head[:12], ", ".join(got["differ"])))
            if got["hidden_present"]:
                raise RuntimeError("repo pack: the template has hidden files: %s" % ", ".join(got["hidden_present"]))
            v = s.judge()
        finally:
            s.close()
    finally:
        client.delete_task(task)
    if v["reward"] is None:
        raise RuntimeError("repo pack: running the checks failed: %s" % (v.get("error") or v.get("stderr")))
    expected = v["details"]["passed"]
    for c in cfg["checks"]:
        if not expected.get(c["name"]):
            log("repo pack: check %s passes nothing at HEAD; it does not count: %s" % (c["name"], v["details"]["output"].get(c["name"], "")[-500:]))
    STATE["expected"] = {k: ids for k, ids in expected.items() if ids}
    if not STATE["expected"]:
        raise RuntimeError("repo pack: no check passes at HEAD")
    log("repo pack: %s at %s, %d source files, %s tests passing" % (cfg["name"], head[:12], len(sources),
        ", ".join("%s %d" % (k, len(ids)) for k, ids in STATE["expected"].items())))


def _checks_text(cfg):
    return "; ".join("`%s`" % c.get("agent_cmd", c["cmd"]) for c in cfg["checks"] if c.get("agent_cmd") or not c["cmd"].startswith("python3 /judge/"))


def _spec(cfg, instruction, files):
    tests = _checks_text(cfg)
    text = instruction + (" The tests: %s." % tests if tests else "") + \
        " Only changes to the existing source files count; the tests are not yours to change." + \
        (" " + cfg["instruction"] if cfg["instruction"] else "")
    return {"instruction": text, "files": files, "judge": judge(cfg), "env": cfg["env"], "limits": cfg["limits"]}


def mutants(cfg, rng):
    """Single-site mutations of the source files, all files mixed: a file without tests gives mutants no test
    catches, which `build` refuses, so they cannot fill the family."""
    found = []
    for path in STATE["sources"]:
        lang = cfg.get("lang") or LANGS.get(os.path.splitext(path)[1])
        try:
            src = blob(cfg, STATE["head"], path).decode()
        except UnicodeDecodeError:
            continue
        if lang is not None:
            found += [(path, src, what, m) for what, m in mutations(src, lang)]
    rng.shuffle(found)
    for path, src, what, mutant in found:
        full = "%s/%s" % (cfg["root"], path)
        text = "There is a bug in %s of the repository at %s: some of its tests fail. Find it and fix it." % (path, cfg["root"])
        yield packkit.Candidate("%s/mutation" % cfg["name"], _spec(cfg, text, [tfile(full, mutant.encode())]), [tfile(full, src.encode())],
                                {"mode": "mutation", "file": path, "mutation": what, "head": STATE["head"]})


def undo(cfg, head, commit, path):
    """`path` at `head` with `commit`'s change to it undone; None if that cannot be done cleanly."""
    r = subprocess.run(["git", "-C", cfg["checkout"], "merge-file", "-p", "--object-id",
                        "%s:%s" % (head, path), "%s:%s" % (commit, path), "%s^:%s" % (commit, path)], capture_output=True)
    if r.returncode == 0:
        return r.stdout
    # a three-way merge refuses changes next to each other (a fix to a function's last line, a function added
    # after it): undo each hunk where its lines and the line before or after them are found once
    now, new, old = (blob(cfg, rev, path) for rev in (head, commit, commit + "^"))
    try:
        now, new, old = (x.decode().splitlines(keepends=True) for x in (now, new, old))
    except UnicodeDecodeError:
        return None
    edits = []
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, new, old, autojunk=False).get_opcodes():
        if tag == "equal":
            continue
        at = None
        for before, after in ((new[max(0, i1 - 1):i1], []), ([], new[i2:i2 + 1])):
            want = before + new[i1:i2] + after
            if not want:
                continue
            hits = [k for k in range(len(now) - len(want) + 1) if now[k:k + len(want)] == want]
            if len(hits) == 1:
                at = hits[0] + len(before)
                break
        if at is None:
            return None
        edits.append((at, at + i2 - i1, old[j1:j2]))
    edits.sort()
    if any(a[1] > b[0] for a, b in zip(edits, edits[1:])):
        return None
    for start, end, lines in reversed(edits):
        now[start:end] = lines
    return "".join(now).encode()


def fixes(cfg, rng):
    """A past commit that changed source files, undone on HEAD: its source changes reverted (a three-way merge),
    every later change kept. Commits that added or removed source files, or whose undoing conflicts, are skipped."""
    head, sources = STATE["head"], set(STATE["sources"])
    log = git(cfg, "log", "--no-merges", "--format=%H", "-n", str(cfg["history"].get("commits", 200)), head).stdout.decode().split()
    rng.shuffle(log)
    for commit in log:
        changed = git(cfg, "diff-tree", "--no-commit-id", "-r", "--name-status", "-z", "%s^" % commit, commit, check=False)
        if changed.returncode != 0:
            continue  # the root commit
        parts = changed.stdout.decode().split("\0")
        entries = list(zip(parts[0::2], parts[1::2]))
        touched = [(st, p) for st, p in entries if p in sources or (_match(p, cfg["include"]) and not _match(p, cfg["exclude"]))]
        if not touched or any(st != "M" or p not in sources for st, p in touched):
            continue
        files, solution = [], []
        for _, p in touched:
            now = blob(cfg, head, p)
            undone = undo(cfg, head, commit, p)
            if undone is None or undone == now:
                break
            files.append(tfile("%s/%s" % (cfg["root"], p), undone))
            solution.append(tfile("%s/%s" % (cfg["root"], p), now))
        else:
            if cfg["history"].get("message", True):
                message = git(cfg, "log", "-1", "--format=%B", commit).stdout.decode().strip()
                text = "Fix this issue in the repository at %s:\n\n%s\n\n" % (cfg["root"], message)
            else:
                text = "Some of the tests of the repository at %s fail. Find the bug and fix it." % cfg["root"]
            yield packkit.Candidate("%s/history" % cfg["name"], _spec(cfg, text, files), solution,
                                    {"mode": "history", "commit": commit, "files": [p for _, p in touched], "head": head})


def candidates(rng):
    cfg = config()
    if "head" not in STATE:
        raise SystemExit("repo pack: prepare() has not run (packkit build runs it)")
    out = {}
    if "mutation" in cfg["modes"]:
        out["%s/mutation" % cfg["name"]] = mutants(cfg, rng)
    if "history" in cfg["modes"]:
        out["%s/history" % cfg["name"]] = fixes(cfg, rng)
    return out
