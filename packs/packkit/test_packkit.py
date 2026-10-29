"""packkit.build against a fake service: it stops at the count asked, gives up on families that never yield a
task, refuses a pack whose generators mix up families, and runs the pack's prepare first. And packkit.mutate.

    PYTHONPATH=packs:sdk/python python3 -m unittest packkit.test_packkit
"""

import itertools
import random
import types
import unittest

import packkit
from packkit.mutate import mutations


class FakeSession:
    def __init__(self, client, task_id):
        self.client, self.task_id, self.files = client, task_id, {}

    def act(self, action):
        self.files[action["path"]] = action["content"]
        return {"exit_code": 0}

    def judge(self):
        # solved when the session holds the task's "answer" file with the right content
        want = self.client.tasks[self.task_id]["answer"]
        return {"reward": 1.0 if self.files.get("/answer") == want else self.client.tasks[self.task_id]["start"], "details": {}}

    def close(self):
        pass


class FakeClient:
    def __init__(self):
        self.tasks, self.deleted, self.n = {}, [], 0

    def create_task(self, spec):
        self.n += 1
        tid = "t%d" % self.n
        self.tasks[tid] = spec
        return tid

    def rollouts(self, task_id, variants):
        return [FakeSession(self, task_id) for _ in variants]

    def delete_task(self, task_id):
        self.deleted.append(task_id)


def candidate(family, start=0.0, answer="42"):
    return packkit.Candidate(family, {"answer": answer, "start": start}, [{"path": "/answer", "content": answer}])


class Build(unittest.TestCase):
    def test_stops_at_the_count_and_gives_up_on_hopeless_families(self):
        def candidates(rng):
            return {"good": (candidate("good") for _ in itertools.count()),
                    # already solved at the start: never a task
                    "hopeless": (candidate("hopeless", start=1.0) for _ in itertools.count())}
        client = FakeClient()
        tasks, refs, short = packkit.build(client, types.SimpleNamespace(candidates=candidates), "tpl", per_family=3, log=lambda *_: None)
        self.assertEqual([t["family"] for t in tasks], ["good"] * 3)
        self.assertEqual(short, {"hopeless": 3})
        self.assertEqual(client.n, 3 + 20 * 3, "the hopeless family stops after 20 x the count")
        self.assertEqual(len(client.deleted), 20 * 3, "refused candidates are deleted")

    def test_a_generator_that_makes_another_family_is_an_error(self):
        def candidates(rng):
            return {"a": iter([candidate("b")]), "b": iter([candidate("b")])}
        with self.assertRaises(ValueError):
            packkit.build(FakeClient(), types.SimpleNamespace(candidates=candidates), "tpl", per_family=1, log=lambda *_: None)

    def test_prepare_runs_before_the_candidates(self):
        seen = []
        def prepare(client, template, log):
            seen.append(("prepare", template))
        def candidates(rng):
            seen.append("candidates")
            return {"a": iter([candidate("a")])}
        packkit.build(FakeClient(), types.SimpleNamespace(prepare=prepare, candidates=candidates), "tpl", per_family=1, log=lambda *_: None)
        self.assertEqual(seen, [("prepare", "tpl"), "candidates"])


class Mutate(unittest.TestCase):
    def kinds(self, src, lang):
        return [d.rsplit(" at line", 1)[0] for d, _ in mutations(src, lang)]

    def test_python_operators_constants_and_conditions(self):
        src = 'def f(a, b):\n    if a > 0 and not b:\n        return a * 2  # a + b\n    return "x-y"\n'
        self.assertEqual(sorted(self.kinds(src, "python")),
                         sorted(["> -> <", "0 -> 1", "and -> or", "not removed", "condition negated", "* -> /", "2 -> 3", "2 -> 1"]))

    def test_python_leaves_unary_and_compound_operators_and_drops_what_does_not_compile(self):
        src = "def f(a, *args, **kw) -> int:\n    a += -1\n    return a ** 2 // 3\n"
        for d, m in mutations(src, "python"):
            compile(m, "m", "exec")
        self.assertEqual(sorted(self.kinds(src, "python")), sorted(["1 -> 2", "1 -> 0", "2 -> 3", "2 -> 1", "3 -> 4", "3 -> 2"]))

    def test_c_leaves_pointers_strings_comments_and_indexes(self):
        src = 'int g(int *p, int n) { if (n < 4) return p[n + 1]; /* a - b */ puts("a+b"); return *p - n; }\n'
        self.assertEqual(sorted(self.kinds(src, "c")), sorted(["< -> >", "4 -> 5", "4 -> 3", "condition negated", "- -> +"]))


if __name__ == "__main__":
    unittest.main()
