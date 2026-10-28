"""Generate the EDA tasks on a service and keep those that are tasks: the starting state scores below 1 (and, for
RTL, compiles), and the reference solution scores 1. Each candidate is checked with two rollouts and the judge,
concurrently.

    SELFDB_ENV_API_KEY=... python3 build_tasks.py BASE_URL TEMPLATE_ID [--rtl-per-design N] [--analog-per-kind N]
        [--seed S] [--workers W] [--out tasks.json] [--references refs.json]

``tasks.json`` lists the tasks (id, family, design or kind, instruction, starting reward); ``refs.json`` holds the
reference solutions, for evaluation on the trainer's side: they are not part of any task.
"""

import argparse
import concurrent.futures
import json
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import analog  # noqa: E402
import rtl  # noqa: E402
from selfdb_env import Client, EnvError  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def judge_source(name):
    with open(os.path.join(HERE, "judges", name)) as f:
        return f.read()


def validate(client, template, spec, ref, meta):
    """(the task's entry in tasks.json, None) if it is a task; else (None, the two verdicts), the task deleted."""
    task_id = client.create_task(dict(spec, template_id=template))
    sessions, keep = [], False
    try:
        sessions = client.rollouts(task_id, [{"label": "start"}, {"label": "reference"}])
        start, solved = sessions
        v0 = start.judge()
        solved.write_file(ref["path"], ref["content"])
        v1 = solved.judge()
        keep = (v0["reward"] is not None and v0["reward"] < 1.0 and v1["reward"] == 1.0
                and (meta["family"] != "rtl" or v0["details"].get("compiled")))
        if not keep:
            return None, {"start": v0, "reference": v1}
        return dict(meta, task_id=task_id, instruction=spec["instruction"], start_reward=v0["reward"]), None
    finally:
        for s in sessions:
            s.close()
        if not keep:
            try:
                client.delete_task(task_id)
            except EnvError:
                pass


def main():
    p = argparse.ArgumentParser()
    p.add_argument("base_url")
    p.add_argument("template_id")
    p.add_argument("--rtl-per-design", type=int, default=3)
    p.add_argument("--analog-per-kind", type=int, default=3)
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--workers", type=int, default=8)
    p.add_argument("--out", default="tasks.json")
    p.add_argument("--references", default="refs.json")
    a = p.parse_args()
    client = Client(a.base_url, os.environ["SELFDB_ENV_API_KEY"])
    rng = random.Random(a.seed)
    rtl_judge, spice_judge = judge_source("rtl_judge.py"), judge_source("spice_judge.py")

    # candidates: for RTL a few more mutants than needed (some compile not, some the testbench cannot tell)
    queue = {}
    for name in rtl.DESIGNS:
        ms = rtl.mutations(rtl.reference(name))
        rng.shuffle(ms)
        queue[("rtl", name)] = [rtl.task(name, m, d, rtl_judge) for d, m in ms]
    for kind in analog.KINDS:
        queue[("analog", kind)] = [analog.task(kind, random.Random(rng.random()), spice_judge) for _ in range(a.analog_per_kind * 2)]
    want = {k: (a.rtl_per_design if k[0] == "rtl" else a.analog_per_kind) for k in queue}

    tasks, refs, rejected = [], {}, 0
    with concurrent.futures.ThreadPoolExecutor(a.workers) as pool:
        while True:
            batch = []
            for k, cands in queue.items():
                have = sum(1 for t in tasks if (t["family"], t.get("design") or t.get("kind")) == k)
                while cands and have + sum(1 for b in batch if b[0] == k) < want[k]:
                    batch.append((k, cands.pop()))
            if not batch:
                break
            futures = [(k, c, pool.submit(validate, client, a.template_id, *c)) for k, c in batch]
            for k, (spec, ref, meta), f in futures:
                entry, why = f.result()
                if entry is None:
                    rejected += 1
                    continue
                tasks.append(entry)
                refs[entry["task_id"]] = ref
    with open(a.out, "w") as f:
        json.dump(tasks, f, indent=1)
    with open(a.references, "w") as f:
        json.dump(refs, f, indent=1)
    short = {k: want[k] - sum(1 for t in tasks if (t["family"], t.get("design") or t.get("kind")) == k) for k in want}
    print("%d tasks (%d candidates rejected) -> %s; short of the count asked: %s"
          % (len(tasks), rejected, a.out, {"%s/%s" % k: n for k, n in short.items() if n > 0} or "none"))


if __name__ == "__main__":
    main()
