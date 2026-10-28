"""What an RTL task's module may contain: synthesizable RTL only (shared by the RTL and FPGA judges).

No system tasks (a module could print the testbench's RESULT line itself), no initial or final blocks, no
compiler directives, and exactly one module, the one named.
"""

import re

# system functions that synthesizable RTL uses
ALLOWED_SYSTEM = {"$signed", "$unsigned", "$clog2"}
FORBIDDEN_WORDS = ("initial", "final", "force", "release", "deassign", "fork", "program", "bind", "class", "package",
                   "interface", "import", "export", "specify")


def strip_comments(src):
    src = re.sub(r"/\*.*?\*/", " ", src, flags=re.S)
    return re.sub(r"//[^\n]*", " ", src)


def check(src, name):
    code = strip_comments(src)
    if '"' in code:
        return "string literals are not allowed"
    if "`" in code:
        return "compiler directives are not allowed"
    for s in re.findall(r"\$[A-Za-z_][A-Za-z0-9_$]*", code):
        if s not in ALLOWED_SYSTEM:
            return "system task or function %s is not allowed" % s
    for w in FORBIDDEN_WORDS:
        if re.search(r"\b%s\b" % w, code):
            return "%s is not allowed" % w
    modules = re.findall(r"\b(?:module|macromodule|primitive)\s+(\w+)", code)
    if modules != [name]:
        return "the file must define exactly one module, %s (found %s)" % (name, modules or "none")
    return None


