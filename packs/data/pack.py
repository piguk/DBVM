"""The data pack for packkit: questions about a small company's data, answered as a CSV table or a JSON report.
See README.md."""

import base64
import itertools
import json
import os
import random

import dataset
import packkit

HERE = os.path.dirname(os.path.abspath(__file__))


def judge_files(answer, kind, ordered, expected, rel=1e-6):
    with open(os.path.join(HERE, "judge.py")) as f:
        judge = f.read()
    return [
        {"path": "/judge/judge.py", "content": judge},
        {"path": "/judge/judgelib.py", "content": packkit.judgelib()},
        {"path": "/judge/spec.json", "content": json.dumps({"answer": answer, "kind": kind, "ordered": ordered, "rel": rel})},
        {"path": "/judge/expected." + kind, "content": expected},
    ]


def task(family, files, instruction, answer, kind, ordered, expected, meta):
    spec = {
        "instruction": instruction,
        "readonly": ["/data"],
        "files": files,
        "judge": {"cmd": ["python3", "/judge/judge.py"], "sees": ["/work"], "timeout_secs": 60,
                  "files": judge_files(answer, kind, ordered, expected)},
    }
    if kind == "csv":
        header = expected.split("\n", 1)[0]
        cheats = [("an empty answer", [{"path": answer, "content": ""}]),
                  ("the header alone", [{"path": answer, "content": header + "\n"}]),
                  ("the answer somewhere else", [{"path": "/tmp/answer.csv", "content": expected}])]
    else:
        cheats = [("an empty object", [{"path": answer, "content": "{}"}]),
                  ("not JSON", [{"path": answer, "content": "total_orders: many"}]),
                  ("the answer somewhere else", [{"path": "/tmp/report.json", "content": expected}])]
    return packkit.Candidate(family=family, spec=spec, solution=[{"path": answer, "content": expected}], meta=meta, cheats=cheats)


def db_files(db):
    return [{"path": "/data/company.db", "content": base64.b64encode(dataset.db_bytes(db)).decode(), "encoding": "base64"}]


def csv_answer(cur):
    return dataset.csv_text([c[0] for c in cur.description], cur.fetchall())


SQL_INTRO = "The SQLite database /data/company.db (read-only) holds a company's data:\n" + dataset.DESCRIPTION + "\n\n"


def revenue_by_region(rng):
    db = dataset.generate(rng)
    year = rng.choice(["2022", "2023", "2024"])
    expected = csv_answer(db.execute(
        "SELECT c.region AS region, ROUND(SUM(i.qty * i.unit_price), 2) AS revenue FROM orders o JOIN customers c ON c.id = o.customer_id "
        "JOIN order_items i ON i.order_id = o.id WHERE o.status = 'completed' AND strftime('%Y', o.order_date) = ? "
        "GROUP BY c.region ORDER BY revenue DESC", (year,)))
    text = SQL_INTRO + ("Write /work/answer.csv: the revenue of %s per customer region, highest first, with the header "
                        "`region,revenue` and the revenue rounded to 2 decimals." % year)
    return task("sql/revenue_by_region", db_files(db), text, "/work/answer.csv", "csv", True, expected, {"year": year})


def top_customers(rng):
    db = dataset.generate(rng)
    year, k = rng.choice(["2022", "2023", "2024"]), rng.choice([3, 5, 10])
    expected = csv_answer(db.execute(
        "SELECT c.id AS customer_id, c.name AS name, ROUND(SUM(i.qty * i.unit_price), 2) AS revenue FROM orders o "
        "JOIN customers c ON c.id = o.customer_id JOIN order_items i ON i.order_id = o.id "
        "WHERE o.status = 'completed' AND strftime('%Y', o.order_date) = ? GROUP BY c.id ORDER BY revenue DESC, c.id LIMIT ?",
        (year, k)))
    text = SQL_INTRO + ("Write /work/answer.csv: the %d customers who brought the most revenue in %s, most first, with the "
                        "header `customer_id,name,revenue` and the revenue rounded to 2 decimals." % (k, year))
    return task("sql/top_customers", db_files(db), text, "/work/answer.csv", "csv", True, expected, {"year": year, "k": k})


def unordered_products(rng):
    for _ in range(100):
        db = dataset.generate(rng)
        year, cat = rng.choice(["2022", "2023", "2024"]), rng.choice(dataset.CATEGORIES)
        cur = db.execute(
            "SELECT p.id AS product_id, p.name AS name FROM products p WHERE p.category = ? AND p.id NOT IN ("
            "SELECT i.product_id FROM order_items i JOIN orders o ON o.id = i.order_id WHERE o.status = 'completed' "
            "AND strftime('%Y', o.order_date) = ?) ORDER BY p.id", (cat, year))
        expected = csv_answer(cur)
        # an empty answer would be the header alone, which an agent could guess
        if expected.count("\n") > 1:
            break
    else:
        raise RuntimeError("no category with unordered products in 100 datasets")
    text = SQL_INTRO + ("Write /work/answer.csv: the products of category `%s` that no completed order of %s contains, by "
                        "product id, with the header `product_id,name`." % (cat, year))
    return task("sql/unordered_products", db_files(db), text, "/work/answer.csv", "csv", True, expected, {"year": year, "category": cat})


def yearly_report(rng):
    db = dataset.generate(rng)
    year = rng.choice(["2022", "2023", "2024"])
    done = "o.status = 'completed' AND strftime('%Y', o.order_date) = ?"
    orders = db.execute("SELECT COUNT(*) FROM orders o WHERE " + done, (year,)).fetchone()[0]
    revenue = db.execute("SELECT SUM(i.qty * i.unit_price) FROM orders o JOIN order_items i ON i.order_id = o.id WHERE " + done, (year,)).fetchone()[0]
    best = db.execute("SELECT CAST(strftime('%m', o.order_date) AS INTEGER) AS m, SUM(i.qty * i.unit_price) AS r FROM orders o "
                      "JOIN order_items i ON i.order_id = o.id WHERE " + done + " GROUP BY m ORDER BY r DESC, m LIMIT 1", (year,)).fetchone()[0]
    expected = json.dumps({"total_orders": orders, "total_revenue": round(revenue, 2), "best_month": best,
                           "avg_order_value": round(revenue / orders, 2)}, indent=1)
    text = SQL_INTRO + ("Write /work/report.json, a JSON object for %s with: total_orders (completed orders), total_revenue "
                        "(rounded to 2 decimals), best_month (1-12, the month with the most revenue) and avg_order_value "
                        "(revenue per completed order, rounded to 2 decimals)." % year)
    return task("report/yearly", db_files(db), text, "/work/report.json", "json", False, expected, {"year": year})


def category_quantities(rng):
    """Spreadsheet work: the data as CSV files only."""
    db = dataset.generate(rng)
    year = rng.choice(["2022", "2023", "2024"])
    files = [{"path": "/data/%s.csv" % t, "content": dataset.table_csv(db, t)} for t in ("products", "orders", "order_items")]
    expected = csv_answer(db.execute(
        "SELECT p.category AS category, SUM(i.qty) AS quantity FROM order_items i JOIN orders o ON o.id = i.order_id "
        "JOIN products p ON p.id = i.product_id WHERE o.status = 'completed' AND strftime('%Y', o.order_date) = ? "
        "GROUP BY p.category ORDER BY p.category", (year,)))
    text = ("The CSV files in /data (read-only) are a company's products(id, name, category, price), orders(id, customer_id, "
            "order_date, status) and order_items(order_id, product_id, qty, unit_price). Write /work/answer.csv: the quantity "
            "sold per product category in %s, counting completed orders only, by category name, with the header "
            "`category,quantity`." % year)
    return task("csv/category_quantities", files, text, "/work/answer.csv", "csv", True, expected, {"year": year})


FAMILIES = [revenue_by_region, top_customers, unordered_products, yearly_report, category_quantities]


def endless(make, rng):
    while True:
        yield make(rng)


def candidates(rng):
    # a generator function per family: a generator expression in the loop would call the last family's `make`
    out = {}
    for make in FAMILIES:
        r = random.Random(rng.random())
        first = make(r)
        out[first.family] = itertools.chain([first], endless(make, r))
    return out
