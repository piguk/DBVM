"""A small company's data, generated from a seed: customers, products, orders and their items, as a SQLite
database and as CSV files."""

import csv
import datetime
import io
import os
import sqlite3
import tempfile

REGIONS = ["north", "south", "east", "west", "central"]
CATEGORIES = ["tools", "garden", "kitchen", "office", "toys"]
STATUSES = ["completed"] * 8 + ["cancelled", "refunded"]

SCHEMA = """
CREATE TABLE customers(id INTEGER PRIMARY KEY, name TEXT NOT NULL, region TEXT NOT NULL, signup_date TEXT NOT NULL);
CREATE TABLE products(id INTEGER PRIMARY KEY, name TEXT NOT NULL, category TEXT NOT NULL, price REAL NOT NULL);
CREATE TABLE orders(id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id), order_date TEXT NOT NULL, status TEXT NOT NULL);
CREATE TABLE order_items(order_id INTEGER NOT NULL REFERENCES orders(id), product_id INTEGER NOT NULL REFERENCES products(id), qty INTEGER NOT NULL, unit_price REAL NOT NULL);
"""

DESCRIPTION = """customers(id, name, region, signup_date)
products(id, name, category, price)
orders(id, customer_id, order_date YYYY-MM-DD, status: completed | cancelled | refunded)
order_items(order_id, product_id, qty, unit_price)
Revenue is the sum of qty * unit_price over the items of completed orders."""


def generate(rng, customers=200, products=150, orders=2000):
    """A connection to an in-memory database of the company."""
    db = sqlite3.connect(":memory:")
    db.executescript(SCHEMA)
    first = ["Ada", "Ben", "Cai", "Dee", "Eli", "Fay", "Gus", "Hao", "Ivy", "Jun", "Kim", "Lea", "Max", "Noa", "Oli", "Pia"]
    last = ["Ng", "Ortiz", "Patel", "Quinn", "Rossi", "Sato", "Tan", "Ueda", "Vega", "Wang", "Xu", "Yilmaz", "Zhou"]
    for i in range(1, customers + 1):
        d = datetime.date(2021, 1, 1) + datetime.timedelta(days=rng.randrange(0, 1000))
        db.execute("INSERT INTO customers VALUES (?,?,?,?)", (i, "%s %s" % (rng.choice(first), rng.choice(last)), rng.choice(REGIONS), d.isoformat()))
    for i in range(1, products + 1):
        db.execute("INSERT INTO products VALUES (?,?,?,?)", (i, "item-%03d" % i, rng.choice(CATEGORIES), round(rng.uniform(2, 200), 2)))
    prices = {pid: price for pid, price in db.execute("SELECT id, price FROM products")}
    # a few products sell a lot, many rarely (some not at all in a year), as in a real catalogue
    ids = sorted(prices)
    rng.shuffle(ids)
    weights = [1.0 / (rank + 1) ** 1.3 for rank in range(len(ids))]
    for i in range(1, orders + 1):
        d = datetime.date(2022, 1, 1) + datetime.timedelta(days=rng.randrange(0, 3 * 365))
        db.execute("INSERT INTO orders VALUES (?,?,?,?)", (i, rng.randrange(1, customers + 1), d.isoformat(), rng.choice(STATUSES)))
        # a product appears at most once in an order
        chosen = set()
        while len(chosen) < rng.randrange(1, 5):
            chosen.add(rng.choices(ids, weights)[0])
        for pid in sorted(chosen):
            discount = rng.choice([1.0, 1.0, 1.0, 0.9, 0.8])
            db.execute("INSERT INTO order_items VALUES (?,?,?,?)", (i, pid, rng.randrange(1, 6), round(prices[pid] * discount, 2)))
    db.commit()
    return db


def db_bytes(db):
    """The database as a SQLite file's bytes."""
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    try:
        out = sqlite3.connect(path)
        db.backup(out)
        out.close()
        with open(path, "rb") as f:
            return f.read()
    finally:
        os.remove(path)


def csv_text(header, rows):
    buf = io.StringIO()
    w = csv.writer(buf, lineterminator="\n")
    w.writerow(header)
    w.writerows(rows)
    return buf.getvalue()


def table_csv(db, table):
    cur = db.execute("SELECT * FROM %s ORDER BY 1" % table)
    return csv_text([c[0] for c in cur.description], cur.fetchall())
