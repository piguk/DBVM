"""Building task packs for the environment service: generate candidate tasks, keep those that are tasks, and
check their judges.

A pack is a directory with a ``pack.py`` that defines ``candidates(rng)``: for each family of tasks (``"rtl/alu"``,
``"sql/top_customers"``), an iterator of ``Candidate``s. Each candidate is a task (its spec without the template),
the files of a solution, and cheats: ways of gaming the judge, each a set of files that must score 0.

    python3 -m packkit build PACK_DIR BASE_URL TEMPLATE_ID [--per-family N] [--seed S] [--workers W]
                             [--families PREFIX,...] [--out tasks.json] [--references refs.json]
    python3 -m packkit check BASE_URL tasks.json refs.json

``build`` keeps a candidate when its starting state scores below 1 (or what the pack's ``start_ok`` accepts) and
its solution scores 1, each checked with a rollout and the judge, several at once; the others are deleted.
``tasks.json`` lists the tasks; ``refs.json`` holds the solutions and cheats, for the trainer's side (never part
of a task). ``check`` takes one task per family: its solution must score 1, every cheat 0, and its read-only paths
must refuse writes. The API key comes from SELFDB_ENV_API_KEY.
"""

import concurrent.futures
import dataclasses
import importlib.util
import json
import os
import random
import sys

from selfdb_env import Client, EnvError

HERE = os.path.dirname(os.path.abspath(__file__))


@dataclasses.dataclass
class Candidate:
    family: str
    spec: dict
    # files that solve the task: [{"path", "content"[, "encoding"]}]
    solution: list
    meta: dict = dataclasses.field(default_factory=dict)
    # [(name, files)] that must score 0
    cheats: list = dataclasses.field(default_factory=list)


def judgelib():
    """judgelib.py, to ship as a judge file: ``{"path": "/judge/judgelib.py", "content": packkit.judgelib()}``."""
    with open(os.path.join(HERE, "judgelib.py")) as f:
        return f.read()


def load(pack_dir):
    spec = importlib.util.spec_from_file_location("pack", os.path.join(pack_dir, "pack.py"))
    module = importlib.util.module_from_spec(spec)
    sys.path.insert(0, pack_dir)
    spec.loader.exec_module(module)
    return module


def _write(session, files):
    for f in files:
        r = session.act(dict({"tool": "write_file"}, **f))
        if r["exit_code"] != 0:
            raise RuntimeError("write %s: %s" % (f["path"], r["stderr"]))


def validate(client, template, c, start_ok):
    """(entry for tasks.json, refs entry) if the candidate is a task, else (None, why); a refused task is deleted."""
    task_id = client.create_task(dict(c.spec, template_id=template))
    sessions, keep = [], False
    try:
        sessions = client.rollouts(task_id, [{"label": "start"}, {"label": "solution"}])
        start, solved = sessions
        v0 = start.judge()
        _write(solved, c.solution)
        v1 = solved.judge()
        keep = v0["reward"] is not None and start_ok(v0, c) and v1["reward"] == 1.0
        if not keep:
            return None, {"start": v0, "solution": v1}
        entry = dict(c.meta, task_id=task_id, family=c.family, instruction=c.spec.get("instruction"), start_reward=v0["reward"])
        return entry, {"solution": c.solution, "cheats": [[name, files] for name, files in c.cheats]}
    finally:
        for s in sessions:
            s.close()
        if not keep:
            try:
                client.delete_task(task_id)
            except EnvError:
                pass


def build(client, pack, template, per_family=3, seed=1, workers=8, log=print, families=None):
    """`families`: prefixes of the families to make (all if None)."""
    rng = random.Random(seed)
    queues = {family: iter(it) for family, it in pack.candidates(rng).items()
              if families is None or any(family.startswith(f) for f in families)}
    start_ok = getattr(pack, "start_ok", lambda verdict, c: verdict["reward"] < 1.0)
    tasks, refs, rejected = [], {}, 0
    have = {f: 0 for f in queues}
    # a family gives up after this many candidates: some generators never stop
    tried = {f: 0 for f in queues}
    max_tries = 20 * per_family
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        while True:
            batch = []
            for family, it in queues.items():
                for _ in range(per_family - have[family]):
                    c = next(it, None) if tried[family] < max_tries else None
                    if c is None:
                        break
                    if c.family != family:
                        raise ValueError("the pack's %r generator made a candidate of family %r" % (family, c.family))
                    tried[family] += 1
                    batch.append(c)
            if not batch:
                break
            for c, f in [(c, pool.submit(validate, client, template, c, start_ok)) for c in batch]:
                entry, ref = f.result()
                if entry is None:
                    rejected += 1
                    continue
                tasks.append(entry)
                refs[entry["task_id"]] = ref
                have[c.family] += 1
            # a family whose candidates ran out stops asking
            queues = {f: it for f, it in queues.items() if have[f] < per_family}
            if all(have[f] >= per_family for f in queues):
                break
    short = {f: per_family - n for f, n in have.items() if n < per_family}
    log("%d tasks (%d candidates rejected); short of the count asked: %s" % (len(tasks), rejected, short or "none"))
    return tasks, refs, short


def check(client, tasks, refs, log=print):
    """One task per family: the solution scores 1, every cheat 0, read-only paths refuse writes."""
    failures, seen = [], set()
    for t in tasks:
        if t["family"] in seen:
            continue
        seen.add(t["family"])
        ref = refs[t["task_id"]]
        spec = client.task(t["task_id"])
        sessions = client.rollouts(t["task_id"], [{"label": "solution"}] + [{"label": name} for name, _ in ref["cheats"]])
        try:
            _write(sessions[0], ref["solution"])
            r = sessions[0].judge()
            if r["reward"] != 1.0:
                failures.append("%s: the solution scores %s: %s" % (t["family"], r["reward"], r["details"]))
            for s, (name, files) in zip(sessions[1:], ref["cheats"]):
                _write(s, files)
                r = s.judge()
                if r["reward"] != 0.0:
                    failures.append("%s: %s scores %s" % (t["family"], name, r["reward"]))
            for ro in spec.get("readonly", []):
                w = sessions[0].write_file(ro.rstrip("/") + "/.packkit-probe", "x")
                if w["exit_code"] == 0:
                    failures.append("%s: %s accepted a write" % (t["family"], ro))
        finally:
            for s in sessions:
                s.close()
    for f in failures:
        log("FAIL " + f)
    log("checked %d families: %s" % (len(seen), "ok" if not failures else "%d failures" % len(failures)))
    return failures


def main(argv):
    import argparse
    p = argparse.ArgumentParser(prog="packkit")
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("pack_dir")
    b.add_argument("base_url")
    b.add_argument("template_id")
    b.add_argument("--per-family", type=int, default=3)
    b.add_argument("--seed", type=int, default=1)
    b.add_argument("--workers", type=int, default=8)
    b.add_argument("--out", default="tasks.json")
    b.add_argument("--references", default="refs.json")
    b.add_argument("--families", help="comma-separated prefixes of the families to make (e.g. rtl/,fpga/)")
    c = sub.add_parser("check")
    c.add_argument("base_url")
    c.add_argument("tasks")
    c.add_argument("references")
    a = p.parse_args(argv)
    key = os.environ.get("SELFDB_ENV_API_KEY")
    if not key:
        sys.exit("packkit: set SELFDB_ENV_API_KEY")
    if a.cmd == "build":
        client = Client(a.base_url, key)
        families = a.families.split(",") if a.families else None
        tasks, refs, short = build(client, load(a.pack_dir), a.template_id, a.per_family, a.seed, a.workers, families=families)
        with open(a.out, "w") as f:
            json.dump(tasks, f, indent=1)
        with open(a.references, "w") as f:
            json.dump(refs, f, indent=1)
        sys.exit(1 if short else 0)
    client = Client(a.base_url, key)
    failures = check(client, json.load(open(a.tasks)), json.load(open(a.references)))
    sys.exit(1 if failures else 0)
