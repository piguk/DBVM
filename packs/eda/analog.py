"""Analog tasks: the agent sizes a subcircuit until its measured metrics meet a specification.

Each kind gives, for a random target: the spec the judge scores against (judges/spice_judge.py), the read-only
measurement deck, a starting circuit that misses the target, and a reference circuit that meets it (kept by
the task builder, never given to the agent).
"""

import json
import math

OPAMP_LIB = """* an ideal operational amplifier: open-loop gain 1e5, no input current
.subckt opamp inp inn out
E1 out 0 inp inn 1e5
.ends
"""


def _fmt(v):
    return "%.6g" % v


def _log_uniform(rng, lo, hi):
    return math.exp(rng.uniform(math.log(lo), math.log(hi)))


def rc_lowpass(rng):
    f0 = _log_uniform(rng, 200.0, 50e3)
    r = 10e3
    c = 1 / (2 * math.pi * r * f0)
    spec = {"kind": "rc_lowpass", "ports": ["in", "out"], "allow": ["R", "C"], "max_elements": 4,
            "ranges": {"R": [100.0, 1e6], "C": [1e-12, 1e-5]},
            "metrics": [{"name": "f3db", "target": f0, "tol": 0.02, "zero_at": 2.0}]}
    deck = """* RC low-pass: the -3 dB frequency
.include /work/circuit.cir
V1 in 0 dc 0 ac 1
X1 in out dut
.control
ac dec 400 1 100meg
meas ac f3db when vdb(out)=-3 fall=1
echo METRIC f3db $&f3db
.endc
.end
"""
    start = ".subckt dut in out\nR1 in out 1k\nC1 out 0 1u\n.ends\n"
    ref = ".subckt dut in out\nR1 in out %s\nC1 out 0 %s\n.ends\n" % (_fmt(r), _fmt(c))
    text = ("Size the RC low-pass filter in /work/circuit.cir (subcircuit `dut`, ports in and out) so that its -3 dB "
            "frequency is %.4g Hz, within 2%%. Use only resistors and capacitors; R between 100 ohm and 1 Mohm, C between "
            "1 pF and 10 uF." % f0)
    return spec, deck, start, ref, text


def divider(rng):
    ratio = rng.uniform(0.1, 0.9)
    imax = rng.choice([20e-6, 50e-6, 100e-6])
    total = 5.0 / imax * 1.5
    spec = {"kind": "divider", "ports": ["in", "out"], "allow": ["R"], "max_elements": 4,
            "ranges": {"R": [10.0, 10e6]},
            "metrics": [{"name": "ratio", "target": ratio, "tol": 0.01, "zero_at": 1.5}, {"name": "iin", "max": imax}]}
    deck = """* resistive divider from 5 V: output ratio and supply current
.include /work/circuit.cir
V1 in 0 dc 5
X1 in out dut
.control
op
let ratio = v(out)/v(in)
let iin = -i(v1)
echo METRIC ratio $&ratio
echo METRIC iin $&iin
.endc
.end
"""
    start = ".subckt dut in out\nR1 in out 1k\nR2 out 0 1k\n.ends\n"
    ref = ".subckt dut in out\nR1 in out %s\nR2 out 0 %s\n.ends\n" % (_fmt(total * (1 - ratio)), _fmt(total * ratio))
    text = ("Design the resistive divider in /work/circuit.cir (subcircuit `dut`, ports in and out; in is driven by 5 V) "
            "so that out/in is %.4g within 1%% and the supply current is at most %g uA. Use only resistors, 10 ohm to "
            "10 Mohm." % (ratio, imax * 1e6))
    return spec, deck, start, ref, text


def rlc_bandpass(rng):
    f0 = _log_uniform(rng, 1e3, 200e3)
    q = rng.uniform(1.5, 8.0)
    r = 1e3
    l = q * r / (2 * math.pi * f0)
    c = 1 / ((2 * math.pi * f0) ** 2 * l)
    spec = {"kind": "rlc_bandpass", "ports": ["in", "out"], "allow": ["R", "L", "C"], "max_elements": 6,
            "ranges": {"R": [1.0, 1e6], "L": [1e-9, 10.0], "C": [1e-12, 1e-3]},
            "metrics": [{"name": "f0", "target": f0, "tol": 0.02, "zero_at": 2.0},
                        {"name": "q", "target": q, "tol": 0.05, "zero_at": 2.0},
                        {"name": "vpk", "min": -1.0}]}
    deck = """* band-pass: centre frequency (geometric mean of the -3 dB points), Q, and the peak gain in dB
.include /work/circuit.cir
V1 in 0 dc 0 ac 1
X1 in out dut
.control
ac dec 400 10 100meg
meas ac vpk max vdb(out)
meas ac flo when vdb(out)=-3 rise=1
meas ac fhi when vdb(out)=-3 fall=1
let f0 = sqrt(flo*fhi)
let q = f0/(fhi-flo)
echo METRIC vpk $&vpk
echo METRIC f0 $&f0
echo METRIC q $&q
.endc
.end
"""
    start = ".subckt dut in out\nL1 in n1 10m\nC1 n1 out 100n\nR1 out 0 100\n.ends\n"
    ref = ".subckt dut in out\nL1 in n1 %s\nC1 n1 out %s\nR1 out 0 %s\n.ends\n" % (_fmt(l), _fmt(c), _fmt(r))
    text = ("Size the band-pass filter in /work/circuit.cir (subcircuit `dut`, ports in and out) for a centre frequency of "
            "%.4g Hz within 2%%, a Q of %.3g within 5%% (centre over the -3 dB bandwidth), and a peak gain of at least "
            "-1 dB. Use only R, L and C." % (f0, q))
    return spec, deck, start, ref, text


def inverting_amp(rng):
    gain = rng.choice([2.0, 5.0, 10.0, 20.0, 47.0])
    rmin = rng.choice([1e3, 10e3, 47e3])
    rin = rmin * 2
    spec = {"kind": "inverting_amp", "ports": ["in", "out"], "allow": ["R", "X"], "subckts": ["opamp"], "max_elements": 5,
            "ranges": {"R": [10.0, 10e6]},
            "metrics": [{"name": "gain", "target": -gain, "tol": 0.01, "zero_at": 1.5}, {"name": "rin", "min": rmin}]}
    deck = """* inverting amplifier at DC: voltage gain and input resistance
.include /tests/opamp.lib
.include /work/circuit.cir
V1 in 0 dc 0.1
X1 in out dut
.control
op
let gain = v(out)/v(in)
let rin = v(in)/(-i(v1))
echo METRIC gain $&gain
echo METRIC rin $&rin
.endc
.end
"""
    start = ".subckt dut in out\nR1 in n1 1k\nR2 n1 out 1k\nX1 0 n1 out opamp\n.ends\n"
    ref = ".subckt dut in out\nR1 in n1 %s\nR2 n1 out %s\nX1 0 n1 out opamp\n.ends\n" % (_fmt(rin), _fmt(rin * gain))
    text = ("Size the inverting amplifier in /work/circuit.cir (subcircuit `dut`, ports in and out) for a DC voltage gain "
            "of -%g within 1%% and an input resistance of at least %g ohm. It may use resistors and the ideal op-amp "
            "`opamp` (ports inp inn out) from /tests/opamp.lib; nothing else." % (gain, rmin))
    return spec, deck, start, ref, text


KINDS = {"rc_lowpass": rc_lowpass, "divider": divider, "rlc_bandpass": rlc_bandpass, "inverting_amp": inverting_amp}


def task(kind, rng, judge_src):
    """(task spec without template_id, reference circuit, metadata) for one random instance of `kind`."""
    spec, deck, start, ref, text = KINDS[kind](rng)
    files = [
        {"path": "/work/circuit.cir", "content": start},
        {"path": "/tests/spec.json", "content": json.dumps(spec, indent=1) + "\n"},
        {"path": "/tests/measure.cir", "content": deck},
        {"path": "/tests/judge.py", "content": judge_src, "mode": 0o755},
    ]
    if "opamp" in spec.get("subckts", []):
        files.append({"path": "/tests/opamp.lib", "content": OPAMP_LIB})
    instruction = text + " Check with `python3 /tests/judge.py`. Do not modify /tests."
    t = {"instruction": instruction, "readonly": ["/tests"], "files": files,
         "judge": {"cmd": ["python3", "/tests/judge.py"], "sees": ["/work"], "timeout_secs": 180}}
    return t, {"path": "/work/circuit.cir", "content": ref}, {"family": "analog", "kind": kind}
