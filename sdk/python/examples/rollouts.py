"""Run a task as several seeded rollouts concurrently, judge them, fork the best one, and start again from
a checkpoint.

    SELFDB_ENV_API_KEY=... python3 rollouts.py BASE_URL TASK_ID

The key comes from the environment, not the command line, where other users could read it.

The task is expected to judge on /app/calc.sh (as in tests/env_api.rs); each rollout tries a different fix.
"""

import asyncio
import os
import sys

from selfdb_env import AsyncClient, Client

FIXES = ["2 + 3", "2 * 3", "3 + 2", "2 - 3"]


async def one(client, task_id, i, fix):
    async with (await client.session(task_id=task_id, label="fix-%d" % i, seed=i)) as s:
        await s.edit_file("/app/calc.sh", "2 - 3", fix) if fix != "2 - 3" else None
        out = await s.bash("/bin/sh /app/calc.sh; echo seed=$SEED")
        verdict = await s.judge()
        return fix, out["output"].split(), verdict["reward"]


async def main(base, key, task_id):
    client = AsyncClient(base, key)
    results = await asyncio.gather(*(one(client, task_id, i, f) for i, f in enumerate(FIXES)))
    for fix, out, reward in results:
        print("%-6s output=%s reward=%s" % (fix, out, reward))
    assert sorted(r for _, _, r in results) == [0.0, 0.0, 1.0, 1.0], results

    # the synchronous client: a batch of actions, a fork from a fixed state, and the trajectory
    sync = Client(base, key)
    a, b = sync.rollouts(task_id, [{"label": "x", "seed": 1}, {"label": "y", "seed": 2}])
    a.edit_file("/app/calc.sh", "2 - 3", "2 + 3")
    batch = sync.act_batch([(a, {"tool": "bash", "command": "/bin/sh /app/calc.sh"}),
                            (b, {"tool": "bash", "command": "/bin/sh /app/calc.sh"})])
    assert [r["output"] for r in batch] == ["5\n", "-1\n"], batch
    kids = a.fork([{"label": "k%d" % i} for i in range(3)])
    assert all(k.judge()["reward"] == 1.0 for k in kids)
    assert [s["step"] for s in a.trajectory() if "response" in s] == [1, 2]

    # a checkpoint after a wrong edit: the session moves on and ends, new attempts start from that step
    b.edit_file("/app/calc.sh", "2 - 3", "2 * 3")
    ckpt = b.checkpoint()
    b.edit_file("/app/calc.sh", "2 * 3", "2 + 3")
    b.close()
    retries = sync.fork_checkpoint(ckpt["id"], [{"label": "retry-%d" % i} for i in range(2)])
    assert [r.bash("cat /app/calc.sh")["output"] for r in retries] == ["echo $((2 * 3))\n"] * 2
    assert [k["step"] for k in sync.checkpoints(b.id)] == [ckpt["step"]] == [2]
    sync.delete_checkpoint(ckpt["id"])
    for s in [a] + kids + retries:
        s.close()
    print("ok")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1], os.environ["SELFDB_ENV_API_KEY"], sys.argv[2]))
