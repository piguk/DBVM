"""Judge of an FPGA timing task: the testbench must pass; then the module, inside a wrapper that registers every
port, is synthesized for an iCE40 HX8K (Yosys), placed and routed (nextpnr, a fixed seed), and scored by its clock.

    python3 /tests/judge.py

Reads /work/rtl/<module>.v and /tests/{spec.json,tb.sv,wrap.v}. reward = (fmax - start) / (target - start), in
[0, 1]; 0 if the testbench fails, the module breaks the RTL rules (rtl_rules.py), or it uses more than max_lc
logic cells. YoWASP is at $SELFDB_YOWASP (default /opt/yowasp).
"""

import json
import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, "/tests")
from rtl_rules import check  # noqa: E402

spec = json.load(open("/tests/spec.json"))
name = spec["module"]
yowasp = os.environ.get("SELFDB_YOWASP", "/opt/yowasp")


def verdict(reward, **details):
    print(json.dumps(dict(reward=max(0.0, min(1.0, reward)), **details)))
    sys.exit(0)


def run(cmd, cwd, timeout, env=None):
    try:
        return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)
    except subprocess.TimeoutExpired:
        verdict(0.0, error="%s timed out" % cmd[0])


try:
    src = open("/work/rtl/%s.v" % name).read()
except OSError as e:
    verdict(0.0, error="cannot read the module: %s" % e)
why = check(src, name)
if why:
    verdict(0.0, error=why)
build = "/tmp/judge-fpga"
shutil.rmtree(build, ignore_errors=True)
os.makedirs(build)
for f in ("tb.sv", "wrap.v"):
    shutil.copy("/tests/" + f, build)
with open(os.path.join(build, "dut.v"), "w") as f:
    f.write(src)

b = run(["verilator", "--binary", "--timing", "-Wno-fatal", "-Wno-lint", "-Wno-style", "--top-module", "tb", "-Mdir", "obj", "tb.sv", "dut.v"], build, 300)
if b.returncode != 0:
    verdict(0.0, compiled=False, error=(b.stderr or b.stdout)[-2000:])
r = run(["./obj/Vtb"], build, 60)
m = re.findall(r"^RESULT (\d+) (\d+)$", r.stdout, flags=re.M)
if len(m) != 1 or int(m[0][1]) == 0 or m[0][0] != m[0][1]:
    verdict(0.0, compiled=True, error="the testbench failed", result=m)

env = dict(os.environ, PYTHONPATH=yowasp)
y = run(["python3", os.path.join(yowasp, "bin", "yowasp-yosys"), "-q", "-p", "synth_ice40 -top ppa_top -json top.json", "dut.v", "wrap.v"], build, 600, env)
if y.returncode != 0:
    verdict(0.0, compiled=True, passed=True, error="synthesis failed: " + (y.stderr or y.stdout)[-1500:])
p = run(["python3", os.path.join(yowasp, "bin", "yowasp-nextpnr-ice40"), "--hx8k", "--package", "ct256", "--json", "top.json",
         "--freq", "10", "--seed", "1", "--report", "report.json"], build, 600, env)
if p.returncode != 0:
    verdict(0.0, compiled=True, passed=True, error="place and route failed: " + (p.stderr or p.stdout)[-1500:])
report = json.load(open(os.path.join(build, "report.json")))
lc = report["utilization"]["ICESTORM_LC"]["used"]
fmax = min(v["achieved"] for v in report["fmax"].values()) if report.get("fmax") else 0.0
if lc > spec["max_lc"]:
    verdict(0.0, passed=True, fmax=fmax, logic_cells=lc, error="%d logic cells: at most %d" % (lc, spec["max_lc"]))
verdict((fmax - spec["start_fmax"]) / (spec["target_fmax"] - spec["start_fmax"]), passed=True, fmax=round(fmax, 2), logic_cells=lc)
