"""Judge of an RTL task: compiles the agent's module with the read-only testbench and runs it.

    python3 /tests/judge.py NAME

Reads /work/rtl/NAME.v and /tests/tb.sv; prints one JSON object with ``reward`` = checks passed / checks run.
The module must be synthesizable RTL: no system tasks (a module could print the testbench's RESULT line itself),
no initial or final blocks, no compiler directives, and exactly one module, NAME. Anything else scores 0.
"""

import json
import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, "/tests")
from rtl_rules import check  # noqa: E402


def verdict(reward, **details):
    print(json.dumps(dict(reward=reward, **details)))
    sys.exit(0)


def main(name):
    try:
        src = open("/work/rtl/%s.v" % name).read()
    except OSError as e:
        verdict(0.0, compiled=False, error="cannot read the module: %s" % e)
    why = check(src, name)
    if why:
        verdict(0.0, compiled=False, error=why)
    build = "/tmp/judge-build"
    shutil.rmtree(build, ignore_errors=True)
    os.makedirs(build)
    shutil.copy("/tests/tb.sv", build)
    with open(os.path.join(build, name + ".v"), "w") as f:
        f.write(src)
    try:
        b = subprocess.run(["verilator", "--binary", "--timing", "-Wno-fatal", "-Wno-lint", "-Wno-style", "--top-module", "tb",
                            "-Mdir", "obj", "tb.sv", name + ".v"], cwd=build, capture_output=True, text=True, timeout=300)
    except subprocess.TimeoutExpired:
        verdict(0.0, compiled=False, error="compilation timed out")
    if b.returncode != 0:
        verdict(0.0, compiled=False, error=(b.stderr or b.stdout)[-2000:])
    try:
        r = subprocess.run(["./obj/Vtb"], cwd=build, capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired:
        verdict(0.0, compiled=True, error="simulation timed out")
    m = re.findall(r"^RESULT (\d+) (\d+)$", r.stdout, flags=re.M)
    if len(m) != 1 or int(m[0][1]) == 0:
        verdict(0.0, compiled=True, error="no result from the testbench", output=r.stdout[-1000:])
    passed, total = int(m[0][0]), int(m[0][1])
    verdict(passed / total, compiled=True, passed=passed, total=total)


if __name__ == "__main__":
    main(sys.argv[1])
