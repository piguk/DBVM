"""A small Python repository with a history, for the repository pack's test (tests/packs.rs):

    python3 make_repo.py DEST

1. the first version: totals with the discount applied twice;
2. "Apply the discount once": the fix, with a test for it;
3. a tax function, with tests;
4. a new module (a commit that adds a file: the pack skips it).

tests/test_hidden.py is the hidden test; repo.json beside this script configures the pack.
"""

import os
import subprocess
import sys

dest = sys.argv[1]

PRICING_1 = '''"""Order totals."""


def subtotal(items):
    """Sum of quantity times unit price, skipping free items."""
    s = 0
    for it in items:
        if it["qty"] > 0 and not it.get("free"):
            s += it["qty"] * it["price"]
    return s


def total(items, discount=0.0):
    """The subtotal less the discount (a fraction), rounded to cents."""
    s = subtotal(items) * (1 - discount)
    return round(s * (1 - discount), 2)
'''

PRICING_2 = PRICING_1.replace("    return round(s * (1 - discount), 2)\n", "    return round(s, 2)\n")

PRICING_3 = PRICING_2 + '''

def tax(amount, rate, threshold=100):
    """Tax on the part of `amount` above `threshold`, rounded to cents."""
    if amount <= threshold:
        return 0.0
    return round((amount - threshold) * rate, 2)
'''

TESTS_1 = '''import unittest

from calc import pricing

ITEMS = [{"qty": 2, "price": 10.0}, {"qty": 1, "price": 5.5}, {"qty": 3, "price": 1.0, "free": True}]


class Subtotal(unittest.TestCase):
    def test_sum(self):
        self.assertEqual(pricing.subtotal(ITEMS), 25.5)

    def test_empty(self):
        self.assertEqual(pricing.subtotal([]), 0)

    def test_zero_quantity(self):
        self.assertEqual(pricing.subtotal([{"qty": 0, "price": 3.0}]), 0)


class Total(unittest.TestCase):
    def test_no_discount(self):
        self.assertEqual(pricing.total(ITEMS), 25.5)
'''

TESTS_2 = TESTS_1 + '''
    def test_discount(self):
        self.assertEqual(pricing.total(ITEMS, 0.1), 22.95)
'''

TESTS_3 = TESTS_2 + '''

class Tax(unittest.TestCase):
    def test_below(self):
        self.assertEqual(pricing.tax(80, 0.2), 0.0)

    def test_above(self):
        self.assertEqual(pricing.tax(150, 0.2), 10.0)
'''

HIDDEN = '''import unittest

from calc import pricing


class Hidden(unittest.TestCase):
    def test_threshold(self):
        self.assertEqual(pricing.tax(100, 0.5), 0.0)
        self.assertEqual(pricing.tax(101, 0.5), 0.5)

    def test_discount_all(self):
        self.assertEqual(pricing.total([{"qty": 1, "price": 9.99}], 1.0), 0.0)
'''

UNITS = '''"""Unit conversions."""


def cents(amount):
    return int(round(amount * 100))
'''


def write(path, text):
    full = os.path.join(dest, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w") as f:
        f.write(text)


def commit(message):
    subprocess.run(["git", "-C", dest, "add", "-A"], check=True)
    subprocess.run(["git", "-C", dest, "-c", "user.name=test", "-c", "user.email=test@example.invalid",
                    "commit", "-q", "-m", message], check=True)


subprocess.run(["git", "init", "-q", dest], check=True)
write("calc/__init__.py", "")
write("calc/pricing.py", PRICING_1)
write("tests/__init__.py", "")
write("tests/test_pricing.py", TESTS_1)
commit("Order totals")
write("calc/pricing.py", PRICING_2)
write("tests/test_pricing.py", TESTS_2)
commit("Apply the discount once\n\ntotal() applied the discount twice: 10% off 25.50 came to 20.66, not 22.95.")
write("calc/pricing.py", PRICING_3)
write("tests/test_pricing.py", TESTS_3)
write("tests/test_hidden.py", HIDDEN)
commit("Tax above a threshold")
write("calc/units.py", UNITS)
commit("Unit conversions")
