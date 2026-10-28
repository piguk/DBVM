"""RTL tasks: a reference module with one injected bug; the agent fixes it against a self-checking testbench.

A mutation changes one site of the reference: an operator (+ to -, & to |, == to !=, << to >>, ...), a constant
(one more or one less), a clock edge, a condition (negated), or a bit (~ or ! removed). Whether a mutant is a
task (it compiles, and the testbench catches it) is decided by running the judge (build_tasks.py), not here.
"""

import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
DESIGNS = ["alu", "counter", "fifo", "lfsr", "prio_enc", "gray", "edge_detect", "shift_reg"]

SWAP = {"+": "-", "-": "+", "&": "|", "|": "&", "^": "&", "<<": ">>", ">>": "<<", "==": "!=", "!=": "==",
        "<": ">", ">": "<", "&&": "||", "||": "&&"}
# longest first; <= (also the non-blocking assignment) and >= are left alone
TOKEN = re.compile(r"<<|>>|==|!=|<=|>=|&&|\|\||[+\-&|^<>]")
NUMBER = re.compile(r"\b(\d+)'([bdh])([0-9a-fA-F_]+)\b|(?<![\w'.\[:])\b(\d+)\b(?![\w'.:\]])")


def reference(name):
    with open(os.path.join(HERE, "rtl", name, name + ".v")) as f:
        return f.read()


def testbench(name):
    with open(os.path.join(HERE, "rtl", name, "tb.sv")) as f:
        return f.read()


def _code_spans(src):
    """(start, end) of the parts of `src` outside comments, in order."""
    spans, i = [], 0
    for m in re.finditer(r"//[^\n]*|/\*.*?\*/", src, flags=re.S):
        spans.append((i, m.start()))
        i = m.end()
    spans.append((i, len(src)))
    return spans


def _in_brackets(src, pos):
    """Whether `pos` is inside [...] (a bit range or index: mutating there changes widths, not behaviour)."""
    depth = 0
    for c in src[:pos]:
        if c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
    return depth > 0


def mutations(src):
    """Every single-site mutation of `src`: (description, mutated source)."""
    out = []
    for lo, hi in _code_spans(src):
        part = src[lo:hi]
        for m in TOKEN.finditer(part):
            op = m.group(0)
            pos = lo + m.start()
            if op not in SWAP or _in_brackets(src, pos):
                continue
            # a sign or the arrow of a part select ([7:0] is skipped above), not a binary operator
            before = src[:pos].rstrip()
            if op == "-" and (not before or before[-1] in "(=,?:"):
                continue
            new = SWAP[op]
            out.append(("%s -> %s at offset %d" % (op, new, pos), src[:pos] + new + src[pos + len(op):]))
        for m in NUMBER.finditer(part):
            pos = lo + m.start()
            if _in_brackets(src, pos):
                continue
            if m.group(1):
                width, base, digits = int(m.group(1)), m.group(2), m.group(3).replace("_", "")
                v = int(digits, {"b": 2, "d": 10, "h": 16}[base])
                for d in (1, -1):
                    nv = (v + d) % (1 << width)
                    text = "%d'%s%s" % (width, base, format(nv, {"b": "b", "d": "d", "h": "x"}[base]))
                    out.append(("%s -> %s" % (m.group(0), text), src[:pos] + text + src[pos + len(m.group(0)):]))
            else:
                v = int(m.group(4))
                for nv in (v + 1, v - 1):
                    if nv < 0:
                        continue
                    out.append(("%d -> %d at offset %d" % (v, nv, pos), src[:pos] + str(nv) + src[pos + len(m.group(0)):]))
        for m in re.finditer(r"\b(posedge|negedge)\b", part):
            pos = lo + m.start()
            new = "negedge" if m.group(1) == "posedge" else "posedge"
            out.append(("%s -> %s" % (m.group(1), new), src[:pos] + new + src[pos + len(m.group(1)):]))
        for m in re.finditer(r"\bif\s*\(", part):
            start = lo + m.end()
            depth, i = 1, start
            while depth and i < len(src):
                depth += {"(": 1, ")": -1}.get(src[i], 0)
                i += 1
            cond = src[start:i - 1]
            out.append(("if (%s) negated" % cond.strip(), src[:start] + "!(" + cond + ")" + src[i - 1:]))
        for m in re.finditer(r"[~!](?=[\w(])", part):
            pos = lo + m.start()
            out.append(("%s removed at offset %d" % (m.group(0), pos), src[:pos] + src[pos + 1:]))
    return out


def task(name, mutant, description, judge_src):
    """(task spec without template_id, reference file, metadata)."""
    files = [
        {"path": "/work/rtl/%s.v" % name, "content": mutant},
        {"path": "/tests/tb.sv", "content": testbench(name)},
        {"path": "/tests/judge.py", "content": judge_src, "mode": 0o755},
    ]
    instruction = ("The Verilog module `%s` in /work/rtl/%s.v does not do what its header comment describes: the "
                   "self-checking testbench /tests/tb.sv fails. Fix the module (synthesizable RTL; no system tasks, "
                   "initial blocks or directives). Check with `python3 /tests/judge.py %s`. Do not modify /tests."
                   % (name, name, name))
    t = {"instruction": instruction, "readonly": ["/tests"], "files": files,
         "judge": {"cmd": ["python3", "/tests/judge.py", name], "sees": ["/work"], "timeout_secs": 600}}
    return t, {"path": "/work/rtl/%s.v" % name, "content": reference(name)}, {"family": "rtl", "design": name, "mutation": description}
