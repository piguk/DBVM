"""FPGA timing tasks: a module computes its result in one cycle; the agent pipelines it for a higher clock on an
iCE40 FPGA (Yosys and nextpnr, from YoWASP), within an area budget.

Every design streams: `in_valid` with operands in, `out_valid` with a result out, results in input order, at most
16 cycles later. The judge (judges/fpga_judge.py) checks that with a self-checking testbench (Verilator), then
synthesizes the module inside a wrapper that registers every port (so the module's own logic depth is what limits
the clock), places and routes it with a fixed seed, and scores the clock between the starting design's and the
target (95% of a pipelined reference's), with logic cells limited to twice the starting design's.

Targets are measured when a task is made: building these tasks needs YoWASP on the building host (YOWASP names
the directory `pip install --target` put yowasp-yosys and yowasp-nextpnr-ice40 in).
"""

import json
import os
import subprocess
import tempfile

DEVICE = ["--hx8k", "--package", "ct256"]
SEED = "1"


def _lanes(n, w, name):
    return ["%s[%d:%d]" % (name, (i + 1) * w - 1, i * w) for i in range(n)]


def dot4(w):
    a, b = _lanes(4, w, "a"), _lanes(4, w, "b")
    yw = 2 * w + 2
    ports = [("a", 4 * w), ("b", 4 * w)]
    base = " + ".join("%s * %s" % (x, y) for x, y in zip(a, b))
    baseline = """// out_valid and y follow in_valid one cycle later: y = a0*b0 + a1*b1 + a2*b2 + a3*b3 (unsigned, W = %d)
module dot4(input clk, input rst, input in_valid, input [%d:0] a, input [%d:0] b, output reg out_valid, output reg [%d:0] y);
  always @(posedge clk) begin
    out_valid <= rst ? 1'b0 : in_valid;
    y <= %s;
  end
endmodule
""" % (w, 4 * w - 1, 4 * w - 1, yw - 1, base)
    reference = """module dot4(input clk, input rst, input in_valid, input [%d:0] a, input [%d:0] b, output reg out_valid, output reg [%d:0] y);
  reg [%d:0] p0, p1, p2, p3; reg [%d:0] s0, s1; reg v1, v2;
  always @(posedge clk) begin
    v1 <= rst ? 1'b0 : in_valid; v2 <= rst ? 1'b0 : v1; out_valid <= rst ? 1'b0 : v2;
    p0 <= %s * %s; p1 <= %s * %s; p2 <= %s * %s; p3 <= %s * %s;
    s0 <= p0 + p1; s1 <= p2 + p3;
    y <= s0 + s1;
  end
endmodule
""" % (4 * w - 1, 4 * w - 1, yw - 1, 2 * w - 1, 2 * w, a[0], b[0], a[1], b[1], a[2], b[2], a[3], b[3])
    return "dot4", ports, yw, baseline, reference


def sumsq4(w):
    x = _lanes(4, w, "x")
    yw = 2 * w + 2
    ports = [("x", 4 * w)]
    baseline = """// out_valid and y follow in_valid one cycle later: y = x0^2 + x1^2 + x2^2 + x3^2 (unsigned, W = %d)
module sumsq4(input clk, input rst, input in_valid, input [%d:0] x, output reg out_valid, output reg [%d:0] y);
  always @(posedge clk) begin
    out_valid <= rst ? 1'b0 : in_valid;
    y <= %s;
  end
endmodule
""" % (w, 4 * w - 1, yw - 1, " + ".join("%s * %s" % (v, v) for v in x))
    reference = """module sumsq4(input clk, input rst, input in_valid, input [%d:0] x, output reg out_valid, output reg [%d:0] y);
  reg [%d:0] q0, q1, q2, q3; reg [%d:0] s0, s1; reg v1, v2;
  always @(posedge clk) begin
    v1 <= rst ? 1'b0 : in_valid; v2 <= rst ? 1'b0 : v1; out_valid <= rst ? 1'b0 : v2;
    q0 <= %s * %s; q1 <= %s * %s; q2 <= %s * %s; q3 <= %s * %s;
    s0 <= q0 + q1; s1 <= q2 + q3;
    y <= s0 + s1;
  end
endmodule
""" % (4 * w - 1, yw - 1, 2 * w - 1, 2 * w, x[0], x[0], x[1], x[1], x[2], x[2], x[3], x[3])
    return "sumsq4", ports, yw, baseline, reference


def max8(w):
    x = _lanes(8, w, "x")
    ports = [("x", 8 * w)]
    chain = x[0]
    for v in x[1:]:
        chain = "((%s) > %s ? (%s) : %s)" % (chain, v, chain, v)
    baseline = """// out_valid and y follow in_valid one cycle later: y = the largest of the eight W-bit lanes of x (unsigned, W = %d)
module max8(input clk, input rst, input in_valid, input [%d:0] x, output reg out_valid, output reg [%d:0] y);
  always @(posedge clk) begin
    out_valid <= rst ? 1'b0 : in_valid;
    y <= %s;
  end
endmodule
""" % (w, 8 * w - 1, w - 1, chain)
    def mx(p, q):
        return "(%s > %s ? %s : %s)" % (p, q, p, q)
    reference = """module max8(input clk, input rst, input in_valid, input [%d:0] x, output reg out_valid, output reg [%d:0] y);
  reg [%d:0] m0, m1, m2, m3, n0, n1; reg v1, v2;
  always @(posedge clk) begin
    v1 <= rst ? 1'b0 : in_valid; v2 <= rst ? 1'b0 : v1; out_valid <= rst ? 1'b0 : v2;
    m0 <= %s; m1 <= %s; m2 <= %s; m3 <= %s;
    n0 <= %s; n1 <= %s;
    y <= %s;
  end
endmodule
""" % (8 * w - 1, w - 1, w - 1, mx(x[0], x[1]), mx(x[2], x[3]), mx(x[4], x[5]), mx(x[6], x[7]),
       mx("m0", "m1"), mx("m2", "m3"), mx("n0", "n1"))
    return "max8", ports, w, baseline, reference


DESIGNS = {"dot4": dot4, "sumsq4": sumsq4, "max8": max8}


def _products(x, y, lanes, yw):
    """The SystemVerilog sum of lane products of x and y (unsigned, widened to yw bits)."""
    return lambda w: " + ".join("%d'(%s[%d +: %d]) * %d'(%s[%d +: %d])" % (yw, x, i * w, w, yw, y, i * w, w) for i in range(lanes))


def _model(name, w, yw):
    """SystemVerilog statements that set `want` (the expected result) from the operands."""
    if name == "dot4":
        return "want = %s;" % _products("a", "b", 4, yw)(w)
    if name == "sumsq4":
        return "want = %s;" % _products("x", "x", 4, yw)(w)
    return "want = 0; for (int i = 0; i < 8; i++) if (x[i*%d +: %d] > want) want = x[i*%d +: %d];" % (w, w, w, w)


def testbench(name, ports, yw, w):
    """A streaming checker: random operands, in_valid now and then, results compared in input order; a result
    with no input pending fails the run."""
    decls = "\n".join("  logic [%d:0] %s;" % (n - 1, p) for p, n in ports)
    conns = ", ".join(".%s(%s)" % (p, p) for p, _ in ports)
    draw = "\n".join("      %s = {%s};" % (p, ", ".join(["rnd()"] * ((n + 15) // 16))) for p, n in ports)
    return """module tb;
  logic clk = 0, rst = 1, in_valid = 0;
%s
  wire out_valid; wire [%d:0] y;
  %s dut(.clk(clk), .rst(rst), .in_valid(in_valid), %s, .out_valid(out_valid), .y(y));
  logic [15:0] r = 16'h4A3D;
  logic [%d:0] q[$]; logic [%d:0] want;
  int sent = 0, pass = 0, extra = 0;
  function automatic logic [15:0] rnd(); r = {r[14:0], r[15] ^ r[13] ^ r[12] ^ r[10]}; return r; endfunction
  always #5 clk = ~clk;
  always @(posedge clk) if (!rst && out_valid) begin
    if (q.size() == 0) extra++;
    else begin if (y === q[0]) pass++; void'(q.pop_front()); end
  end
  initial begin
    repeat (3) @(negedge clk);
    rst = 0;
    for (int i = 0; i < 600; i++) begin
      @(negedge clk);
%s
      in_valid = (rnd() & 3) != 0;
      if (in_valid) begin
        %s
        q.push_back(want); sent++;
      end
    end
    @(negedge clk) in_valid = 0;
    repeat (20) @(negedge clk);
    $display("RESULT %%0d %%0d", extra == 0 ? pass : 0, sent);
    $finish;
  end
endmodule
""" % (decls, yw - 1, name, conns, yw - 1, yw - 1, draw, _model(name, w, yw))


def wrapper(name, ports, yw):
    """A top that registers every port of the module, for timing: its own logic is what limits the clock."""
    ins = ", ".join("input [%d:0] %s" % (n - 1, p) for p, n in ports)
    regs = "\n".join("  reg [%d:0] r_%s; always @(posedge clk) r_%s <= %s;" % (n - 1, p, p, p) for p, n in ports)
    conns = ", ".join(".%s(r_%s)" % (p, p) for p, _ in ports)
    return """module ppa_top(input clk, input rst, input in_valid, %s, output reg out_valid, output reg [%d:0] y);
  reg r_rst, r_v; always @(posedge clk) begin r_rst <= rst; r_v <= in_valid; end
%s
  wire w_v; wire [%d:0] w_y;
  always @(posedge clk) begin out_valid <= w_v; y <= w_y; end
  %s dut(.clk(clk), .rst(r_rst), .in_valid(r_v), %s, .out_valid(w_v), .y(w_y));
endmodule
""" % (ins, yw - 1, regs, yw - 1, name, conns)


def measure(src, name, ports, yw, yowasp):
    """(logic cells, fmax in MHz) of `src` inside the wrapper, with the judge's flow."""
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "dut.v"), "w") as f:
            f.write(src)
        with open(os.path.join(d, "wrap.v"), "w") as f:
            f.write(wrapper(name, ports, yw))
        env = dict(os.environ, PYTHONPATH=yowasp)
        py = ["python3", os.path.join(yowasp, "bin", "yowasp-yosys")]
        subprocess.run(py + ["-q", "-p", "synth_ice40 -top ppa_top -json top.json", "dut.v", "wrap.v"], cwd=d, env=env, check=True, capture_output=True)
        subprocess.run(["python3", os.path.join(yowasp, "bin", "yowasp-nextpnr-ice40")] + DEVICE +
                       ["--json", "top.json", "--freq", "10", "--seed", SEED, "--report", "report.json"], cwd=d, env=env, check=True, capture_output=True)
        r = json.load(open(os.path.join(d, "report.json")))
        return r["utilization"]["ICESTORM_LC"]["used"], min(v["achieved"] for v in r["fmax"].values())


def task(kind, w, judge_src, rules_src, yowasp):
    """(task spec without template_id, reference file, metadata), targets measured with YoWASP."""
    name, ports, yw, baseline, reference = DESIGNS[kind](w)
    lc0, f0 = measure(baseline, name, ports, yw, yowasp)
    lc1, f1 = measure(reference, name, ports, yw, yowasp)
    spec = {"module": name, "ports": ports, "width": yw, "start_fmax": f0, "target_fmax": 0.95 * f1, "max_lc": 2 * lc0}
    path = "/work/rtl/%s.v" % name
    files = [
        {"path": path, "content": baseline},
        {"path": "/tests/tb.sv", "content": testbench(name, ports, yw, w)},
        {"path": "/tests/wrap.v", "content": wrapper(name, ports, yw)},
        {"path": "/tests/spec.json", "content": json.dumps(spec, indent=1)},
        {"path": "/tests/judge.py", "content": judge_src, "mode": 0o755},
        {"path": "/tests/rtl_rules.py", "content": rules_src},
    ]
    instruction = ("The module `%s` in /work/rtl/%s.v computes its result in one clock cycle, which limits the clock on an "
                   "iCE40 HX8K FPGA to about %.0f MHz. Make it reach %.0f MHz: pipeline it. It must keep its ports and "
                   "streaming behaviour (a result for every in_valid, in input order, out_valid with it, at most 16 cycles "
                   "later) and use at most %d logic cells. The reward rises with the clock from %.0f to %.0f MHz. "
                   "Check with `python3 /tests/judge.py` (it runs the testbench, Yosys and nextpnr). Do not modify /tests."
                   % (name, name, f0, 0.95 * f1, 2 * lc0, f0, 0.95 * f1))
    t = {"instruction": instruction, "readonly": ["/tests"], "files": files, "limits": {"memory_mb": 4096},
         "judge": {"cmd": ["python3", "/tests/judge.py"], "sees": ["/work"], "timeout_secs": 900}}
    return t, {"path": path, "content": reference}, {"family": "fpga", "design": kind, "width": w,
                                                     "start_fmax": round(f0, 1), "target_fmax": round(0.95 * f1, 1), "max_lc": 2 * lc0}
