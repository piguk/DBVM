"""Freeze a template whose YoWASP compile cache is warm: YoWASP compiles Yosys and nextpnr (WebAssembly) on their
first run, which would otherwise happen in every rollout and every judge run.

    SELFDB_ENV_API_KEY=... python3 warm_fpga.py BASE_URL TEMPLATE_ID [NAME]

Prints the new template's id.
"""

import os
import sys

from selfdb_env import Client

DESIGN = "module t(input clk, input [3:0] a, output reg [3:0] y); always @(posedge clk) y <= a + 1; endmodule\n"


def main(base, template, name="eda-fpga"):
    client = Client(base, os.environ["SELFDB_ENV_API_KEY"])
    # compiling Yosys' WebAssembly takes about 2.6 GB (measured), more than a session's default 2 GB
    s = client.session(template_id=template, limits={"memory_mb": 6144})
    try:
        s.write_file("/tmp/warm/t.v", DESIGN)
        r = s.bash("cd /tmp/warm && export PYTHONPATH=/opt/yowasp && python3 /opt/yowasp/bin/yowasp-yosys -q -p 'synth_ice40 -top t -json t.json' t.v "
                   "&& python3 /opt/yowasp/bin/yowasp-nextpnr-ice40 --hx8k --package ct256 --json t.json --freq 10 > /dev/null 2>&1 && echo warm",
                   timeout_secs=1800)
        if "warm" not in r["output"]:
            sys.exit("warming failed: %s %s" % (r["output"], r["stderr"]))
        s.bash("rm -rf /tmp/warm")
        print(s.freeze(name)["id"])
    finally:
        s.close()


if __name__ == "__main__":
    main(*sys.argv[1:4])
