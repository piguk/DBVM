"""Check built EDA tasks on a service: each family's reference solution scores 1, and known ways of gaming the
judges score 0 (a module printing the testbench's result line, an extra module, directives; a source holding
the output at the target, parameters, values out of range). /tests must refuse writes.

    SELFDB_ENV_API_KEY=... python3 check_pack.py BASE_URL tasks.json refs.json
"""

import json
import os
import sys

from selfdb_env import Client

RTL_CHEATS = [
    ("prints the result itself", lambda name, ref: ref.replace("endmodule", 'final $display("RESULT 9999 9999");\nendmodule')),
    ("a second module", lambda name, ref: ref + "\nmodule helper(input a, output b); assign b = a; endmodule\n"),
    ("a directive", lambda name, ref: "`define X 1\n" + ref),
    ("an initial block", lambda name, ref: ref.replace("endmodule", "initial begin end\nendmodule")),
]

ANALOG_CHEATS = [
    ("a source at the output", lambda ref: ref.replace(".ends", "Vfix out 0 dc 1\n.ends")),
    ("a parameter", lambda ref: ".param r=1k\n" + ref),
    ("an expression", lambda ref: ref.replace(".ends", "R9 out 0 {1k*2}\n.ends")),
    ("an include", lambda ref: ref.replace(".ends", ".include /tests/opamp.lib\n.ends")),
    ("a value out of range", lambda ref: ref.replace(".ends", "R9 out 0 1e15\n.ends")),
]


def main(base, tasks_path, refs_path):
    client = Client(base, os.environ["SELFDB_ENV_API_KEY"])
    tasks, refs = json.load(open(tasks_path)), json.load(open(refs_path))
    failures = []
    seen = set()
    for t in tasks:
        key = (t["family"], t.get("design") or t.get("kind"))
        if t["family"] == "rtl" and "rtl" in {k[0] for k in seen}:
            continue  # the RTL judge is the same for every design: one design is enough for the cheats
        if key in seen:
            continue
        seen.add(key)
        ref = refs[t["task_id"]]
        cheats = RTL_CHEATS if t["family"] == "rtl" else ANALOG_CHEATS
        sessions = client.rollouts(t["task_id"], [{"label": "reference"}] + [{"label": c[0]} for c in cheats])
        try:
            sessions[0].write_file(ref["path"], ref["content"])
            r = sessions[0].judge()
            if r["reward"] != 1.0:
                failures.append("%s: the reference scores %s: %s" % (key, r["reward"], r["details"]))
            for s, (what, make) in zip(sessions[1:], cheats):
                content = make(t.get("design"), ref["content"]) if t["family"] == "rtl" else make(ref["content"])
                s.write_file(ref["path"], content)
                r = s.judge()
                if r["reward"] != 0.0:
                    failures.append("%s: %s scores %s" % (key, what, r["reward"]))
            w = sessions[0].write_file("/tests/judge.py", "print('{\"reward\": 1}')")
            if w["exit_code"] == 0:
                failures.append("%s: /tests accepted a write" % (key,))
        finally:
            for s in sessions:
                s.close()
    for f in failures:
        print("FAIL", f)
    print("checked %d task kinds: %s" % (len(seen), "ok" if not failures else "%d failures" % len(failures)))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main(*sys.argv[1:4])
