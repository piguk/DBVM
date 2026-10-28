"""Judge of a data task: compares the agent's answer with the expected one (both hidden from the agent: this
file, judgelib.py and expected.* are judge files).

    python3 /judge/judge.py

/judge/spec.json says where the answer goes, its kind (csv or json), whether row order matters, and the
tolerance. A CSV answer needs the expected header exactly; then reward is the share of expected rows it has,
less for extra rows. A JSON answer scores the share of expected values it has.
"""

import json
import sys

sys.path.insert(0, "/judge")
from judgelib import compare_json, compare_tables, read_csv, verdict  # noqa: E402

spec = json.load(open("/judge/spec.json"))
if spec["kind"] == "csv":
    header, rows = read_csv(spec["answer"])
    want_header, want_rows = read_csv("/judge/expected.csv")
    if header is None:
        verdict(0.0, error="no answer at %s" % spec["answer"])
    if [h.lower() for h in header] != [h.lower() for h in want_header]:
        verdict(0.0, error="the header must be: %s" % ",".join(want_header), header=header)
    verdict(compare_tables(rows, want_rows, ordered=spec["ordered"], rel=spec["rel"]), rows=len(rows), expected_rows=len(want_rows))
else:
    try:
        got = json.load(open(spec["answer"]))
    except (OSError, ValueError) as e:
        verdict(0.0, error="no JSON answer at %s: %s" % (spec["answer"], e))
    verdict(compare_json(got, json.load(open("/judge/expected.json")), rel=spec["rel"]))
