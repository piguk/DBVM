"""The EDA pack for packkit: RTL repair (Verilator) and analog sizing (ngspice). See README.md."""

import os
import random

import analog
import fpga
import packkit
import rtl

HERE = os.path.dirname(os.path.abspath(__file__))

# ways of gaming the RTL judge: each must score 0
RTL_CHEATS = [
    ("prints the result itself", lambda ref: ref.replace("endmodule", 'final $display("RESULT 9999 9999");\nendmodule')),
    ("a second module", lambda ref: ref + "\nmodule helper(input a, output b); assign b = a; endmodule\n"),
    ("a directive", lambda ref: "`define X 1\n" + ref),
    ("an initial block", lambda ref: ref.replace("endmodule", "initial begin end\nendmodule")),
]

# and the analog judge
ANALOG_CHEATS = [
    ("a source at the output", lambda ref: ref.replace(".ends", "Vfix out 0 dc 1\n.ends")),
    ("a parameter", lambda ref: ".param r=1k\n" + ref),
    ("an expression", lambda ref: ref.replace(".ends", "R9 out 0 {1k*2}\n.ends")),
    ("an include", lambda ref: ref.replace(".ends", ".include /tests/opamp.lib\n.ends")),
    ("a value out of range", lambda ref: ref.replace(".ends", "R9 out 0 1e15\n.ends")),
]


def judge_source(name):
    with open(os.path.join(HERE, "judges", name)) as f:
        return f.read()


def candidate(family, spec, ref, meta, cheats):
    return packkit.Candidate(family=family, spec=spec, solution=[ref], meta=meta,
                             cheats=[(name, [{"path": ref["path"], "content": make(ref["content"])}]) for name, make in cheats])


def rtl_candidates(name, mutations, judge, rules):
    for d, m in mutations:
        yield candidate("rtl/" + name, *rtl.task(name, m, d, judge, rules), RTL_CHEATS)


# the FPGA judge's own: a pipelined module that computes something else
FPGA_CHEATS = RTL_CHEATS + [("a wrong result, fast", lambda ref: ref.replace("y <=", "y <= 0 &", 1))]


def fpga_candidates(kind, rng, judge, rules, yowasp):
    while True:
        yield candidate("fpga/" + kind, *fpga.task(kind, rng.choice([6, 8, 10, 12]), judge, rules, yowasp), FPGA_CHEATS)


def analog_candidates(kind, rng, judge):
    while True:
        yield candidate("analog/" + kind, *analog.task(kind, random.Random(rng.random()), judge), ANALOG_CHEATS)


def candidates(rng):
    # generator functions, not generator expressions in the loop: those would read `name` and `kind` when
    # first advanced, after the loop, and every family would make the last one's tasks
    rtl_judge, spice_judge, rules = judge_source("rtl_judge.py"), judge_source("spice_judge.py"), judge_source("rtl_rules.py")
    out = {}
    for name in rtl.DESIGNS:
        ms = rtl.mutations(rtl.reference(name))
        rng.shuffle(ms)
        out["rtl/" + name] = rtl_candidates(name, ms, rtl_judge, rules)
    for kind in analog.KINDS:
        out["analog/" + kind] = analog_candidates(kind, random.Random(rng.random()), spice_judge)
    # the FPGA tasks' targets are measured here, with YoWASP on this host (fpga.py)
    yowasp = os.environ.get("YOWASP")
    if yowasp:
        for kind in fpga.DESIGNS:
            out["fpga/" + kind] = fpga_candidates(kind, random.Random(rng.random()), judge_source("fpga_judge.py"), rules, yowasp)
    else:
        print("packs/eda: no YOWASP: no fpga tasks (see fpga.py)")
    return out


def start_ok(verdict, c):
    """Below 1 at the start; an RTL mutant must also compile (else it is a syntax exercise, not a bug), and an FPGA
    task's starting design must pass its testbench (the task is its clock, not a bug)."""
    if verdict["reward"] >= 1.0:
        return False
    if c.family.startswith("rtl/"):
        return bool(verdict["details"].get("compiled"))
    if c.family.startswith("fpga/"):
        return bool(verdict["details"].get("passed"))
    return True
