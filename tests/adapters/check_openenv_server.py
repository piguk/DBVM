"""The OpenEnv adapter behind OpenEnv's own server (create_app, uvicorn) and client (GenericEnvClient): what a
trainer that speaks OpenEnv sees. Needs openenv-core (run by tests/env_adapters.rs with SELFDB_ADAPTER_PYTHON).

    SELFDB_ENV_API_KEY=... python3 check_openenv_server.py BASE_URL TASK_ID
"""

import os
import socket
import sys
import threading
import time

import uvicorn
from openenv.core import GenericAction, GenericEnvClient
from selfdb_env import Client
from selfdb_env.openenv import create_app


def main(base, task_id):
    client = Client(base, os.environ["SELFDB_ENV_API_KEY"])
    spec = client.task(task_id)
    spec["instruction"] = "Make /app/calc.sh print 5."
    task = client.create_task(spec)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    server = uvicorn.Server(uvicorn.Config(create_app(task, base_url=base, max_steps=5), host="127.0.0.1", port=port, log_level="warning"))
    threading.Thread(target=server.run, daemon=True).start()
    for _ in range(100):
        if server.started:
            break
        time.sleep(0.1)
    env = GenericEnvClient("http://127.0.0.1:%d" % port).sync()
    with env:
        r = env.reset(seed=4)
        assert r.observation["output"] == "Make /app/calc.sh print 5." and not r.done, r
        r = env.step(GenericAction(tool="bash", command="echo $SEED; cat /app/calc.sh"))
        assert r.observation["output"] == "4\necho $((2 - 3))\n", r
        r = env.step(GenericAction(tool="edit_file", path="/app/calc.sh", old="2 - 3", new="2 + 3"))
        assert r.observation["changes"]["modified"] == ["/app/calc.sh"], r
        r = env.step(GenericAction(tool="submit"))
        assert r.done and r.reward == 1.0, r
    server.should_exit = True
    print("ok")


if __name__ == "__main__":
    main(*sys.argv[1:3])
