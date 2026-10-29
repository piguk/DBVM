"""Run Python unittest discovery and write a JUnit XML report: the repository pack's judge reads test results from
JUnit files, and unittest has no such output of its own.

    python3 /judge/unittest_junit.py -o /tmp/unit.xml [-s START] [-t TOP] [-p PATTERN]

The exit code is unittest's (0 when every test passed).
"""

import argparse
import os
import sys
import time
import unittest
import xml.etree.ElementTree as ET


class Result(unittest.TextTestResult):
    def __init__(self, *a, **kw):
        super().__init__(*a, **kw)
        self.cases = []
        self._t0 = 0.0

    def startTest(self, test):
        self._t0 = time.monotonic()
        super().startTest(test)

    def _case(self, test, outcome, text=""):
        cls, _, name = test.id().rpartition(".")
        self.cases.append((cls, name, outcome, text, time.monotonic() - self._t0))

    def addSuccess(self, test):
        super().addSuccess(test)
        self._case(test, "passed")

    def addFailure(self, test, err):
        super().addFailure(test, err)
        self._case(test, "failure", self.failures[-1][1])

    def addError(self, test, err):
        super().addError(test, err)
        self._case(test, "error", self.errors[-1][1])

    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        self._case(test, "skipped", reason)

    def addExpectedFailure(self, test, err):
        super().addExpectedFailure(test, err)
        self._case(test, "passed")

    def addUnexpectedSuccess(self, test):
        super().addUnexpectedSuccess(test)
        self._case(test, "failure", "unexpected success")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("-o", "--output", required=True)
    p.add_argument("-s", "--start", default=".")
    p.add_argument("-t", "--top", default=None)
    p.add_argument("-p", "--pattern", default="test*.py")
    a = p.parse_args()
    top = os.path.abspath(a.top or a.start)
    sys.path.insert(0, top)
    suite = unittest.defaultTestLoader.discover(a.start, a.pattern, top)
    result = unittest.TextTestRunner(resultclass=Result, verbosity=1).run(suite)
    root = ET.Element("testsuite", name="unittest", tests=str(len(result.cases)),
                      failures=str(len(result.failures)), errors=str(len(result.errors)), skipped=str(len(result.skipped)))
    for cls, name, outcome, text, secs in result.cases:
        case = ET.SubElement(root, "testcase", classname=cls, name=name, time="%.3f" % secs)
        if outcome != "passed":
            ET.SubElement(case, outcome, message=text.strip().splitlines()[-1][:200] if text.strip() else "").text = text
    ET.ElementTree(root).write(a.output, encoding="utf-8", xml_declaration=True)
    sys.exit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
