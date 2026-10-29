"""Single-site mutations of source code, for tasks that ask an agent to find and fix a bug: an operator swapped
(+ to -, < to >, == to !=, and to or), a number off by one, a boolean flipped, a condition negated, a `not` or `!`
dropped. Strings and comments are left alone.

`lang` is "python" or "c" (C, C++, Java, JavaScript, Go, Rust and the like: `//` and `/* */` comments, `if (...)`
conditions). Not Verilog, where `<=` is an assignment (packs/eda/rtl.py has its own). Python mutants that do not
compile are left out; for other languages a mutant that does not build is a task too, if an easy one.
"""

import re

SWAP = {"+": "-", "-": "+", "*": "/", "/": "*", "&": "|", "|": "&", "^": "&", "<<": ">>", ">>": "<<",
        "==": "!=", "!=": "==", "<": ">", ">": "<", "<=": ">", ">=": "<", "&&": "||", "||": "&&"}
# longest first
TOKEN = re.compile(r"<<|>>|==|!=|<=|>=|&&|\|\||[+\-*/&|^<>]")
NUMBER = re.compile(r"(?<![\w.])(\d+)(?![\w.])")
WORDS = {"python": {"and": "or", "or": "and", "True": "False", "False": "True"},
         "c": {"true": "false", "false": "true"}}


def _skipped(src, lang):
    """(start, end) of the strings and comments of `src`, in order."""
    if lang == "python":
        pattern = r'"""(?:.|\n)*?"""|\'\'\'(?:.|\n)*?\'\'\'|"(?:\\.|[^"\\\n])*"|\'(?:\\.|[^\'\\\n])*\'|#[^\n]*'
    else:
        pattern = r'"(?:\\.|[^"\\\n])*"|\'(?:\\.|[^\'\\\n])\'|//[^\n]*|/\*(?:.|\n)*?\*/'
    return [(m.start(), m.end()) for m in re.finditer(pattern, src)]


def _code(src, lang):
    """(start, end) of the parts of `src` that are code."""
    out, i = [], 0
    for a, b in _skipped(src, lang):
        out.append((i, a))
        i = b
    out.append((i, len(src)))
    return out


def _in_brackets(src, pos):
    """Inside [...]: an index or a slice, where a change is more often a crash than a bug."""
    depth = 0
    for c in src[:pos]:
        depth += {"[": 1, "]": -1}.get(c, 0)
    return depth > 0


def mutations(src, lang):
    """Every single-site mutation of `src`: (description, mutated source)."""
    out = []
    def put(pos, old, new, what):
        line = src.count("\n", 0, pos) + 1
        out.append(("%s at line %d" % (what, line), src[:pos] + new + src[pos + len(old):]))
    for lo, hi in _code(src, lang):
        part = src[lo:hi]
        for m in TOKEN.finditer(part):
            op, pos = m.group(0), lo + m.start()
            if op not in SWAP or _in_brackets(src, pos):
                continue
            before = src[:pos].rstrip()
            # unary: a sign, a dereference, an address, `*args`
            if op in "+-*&" and (not before or before[-1] in "(=,[{:?<>!&|+-*/%^~;" or re.search(r"\b(return|in|yield|else|case)$", before)):
                continue
            prev, nxt = src[pos - 1: pos], src[pos + len(op): pos + len(op) + 1]
            # `+=`, `->`, `=>`, `**`, `//`, `++`, `<<=`
            # `int *p`, `T &r`: spaced before and not after
            if lang == "c" and op in "*&" and prev.isspace() and nxt and not nxt.isspace():
                continue
            if nxt in ("=", ">") or (nxt and nxt in "+-*/<>&|") or (prev and prev in "=-+*/<>&|!"):
                continue
            if lang == "c" and op in ("<", ">") and re.search(r"\w$", before) and re.match(r"\s*\w+\s*>", src[pos:]):
                continue  # a template or generic argument list
            put(pos, op, SWAP[op], "%s -> %s" % (op, SWAP[op]))
        for m in NUMBER.finditer(part):
            pos, v = lo + m.start(), int(m.group(1))
            if _in_brackets(src, pos):
                continue
            for nv in (v + 1, v - 1):
                if nv >= 0:
                    put(pos, m.group(1), str(nv), "%d -> %d" % (v, nv))
        for word, new in WORDS[lang].items():
            for m in re.finditer(r"\b%s\b" % word, part):
                put(lo + m.start(), word, new, "%s -> %s" % (word, new))
        if lang == "python":
            for m in re.finditer(r"\b(if|elif|while)\s+(.+?):\s*$", part, flags=re.M):
                cond = m.group(2)
                pos = lo + m.start(2)
                out.append(("condition negated at line %d" % (src.count("\n", 0, pos) + 1), src[:pos] + "not (" + cond + ")" + src[pos + len(cond):]))
            for m in re.finditer(r"\bnot\s+", part):
                put(lo + m.start(), m.group(0), "", "not removed")
        else:
            for m in re.finditer(r"\b(if|while)\s*\(", part):
                start = lo + m.end()
                depth, i = 1, start
                while depth and i < len(src):
                    depth += {"(": 1, ")": -1}.get(src[i], 0)
                    i += 1
                cond = src[start:i - 1]
                out.append(("condition negated at line %d" % (src.count("\n", 0, start) + 1), src[:start] + "!(" + cond + ")" + src[i - 1:]))
            for m in re.finditer(r"!(?=[\w(])", part):
                put(lo + m.start(), "!", "", "! removed")
    if lang == "python":
        out = [(d, m) for d, m in out if _compiles(m)]
    return out


def _compiles(src):
    try:
        compile(src, "<mutant>", "exec")
        return True
    except (SyntaxError, ValueError):
        return False
