"""The Harbor and OpenEnv adapters against a running service (run by tests/env_adapters.rs, with the stand-ins
in tests/adapter_stubs on the path).

    SELFDB_ENV_API_KEY=... python3 check_adapters.py BASE_URL TASK_ID SCRATCH_DIR
"""

import asyncio
import hashlib
import os
import sys
from pathlib import Path

from harbor.models.task.config import EnvironmentConfig, NetworkMode, NetworkPolicy
from selfdb_env import Client
from selfdb_env.harbor import SelfdbEnvironment as HarborEnv
from selfdb_env.openenv import SelfdbAction, SelfdbEnvironment as OpenEnv


def harbor_env(base, scratch, policy, **kw):
    return HarborEnv(environment_dir=scratch, environment_name="calc", session_id="trial-1", trial_paths=None,
                     task_env_config=EnvironmentConfig(workdir="/app"), persistent_env={"P": "p q"},
                     network_policy=policy, base_url=base, **kw)


async def check_harbor(base, scratch):
    hosts = harbor_env(base, scratch, NetworkPolicy(NetworkMode.ALLOWLIST, ["*.example.com", "pypi.org"]), template="calc", egress=["x.org"])
    assert hosts._hosts() == ["x.org", ".example.com", "pypi.org"], hosts._hosts()
    assert harbor_env(base, scratch, NetworkPolicy(NetworkMode.NO_NETWORK), template="calc", egress=["x.org"])._hosts() == []

    env = harbor_env(base, scratch, NetworkPolicy(NetworkMode.NO_NETWORK), template="calc")
    await env.start(force_build=False)
    r = await env.exec('echo "$P" $X; pwd', env={"X": "x"})
    assert (r.return_code, r.stdout) == (0, "p q x\n/app\n"), vars(r)
    r = await env.exec("exit 3")
    assert r.return_code == 3
    r = await env.exec("sleep 5", timeout_sec=1)
    assert r.return_code == 124, vars(r)
    for bad in [lambda: env.exec("true", env={"A;rm": "x"}), lambda: env.exec("true", user="agent")]:
        try:
            await bad()
            raise AssertionError("refused")
        except (ValueError, RuntimeError):
            pass

    # files: any bytes, modes, directories with links, reads in several chunks
    local = scratch / "up"
    (local / "sub").mkdir(parents=True)
    blob = os.urandom(2_500_000)
    (local / "sub" / "big.bin").write_bytes(blob)
    (local / "run.sh").write_text("echo ran\n")
    (local / "run.sh").chmod(0o755)
    (local / "link").symlink_to("sub/big.bin")
    await env.upload_dir(local, "/work")
    r = await env.exec("/work/run.sh; ls -l /work/run.sh | cut -c1-10; readlink /work/link")
    assert r.stdout == "ran\n-rwxr-xr-x\nsub/big.bin\n", r.stdout
    down = scratch / "down"
    await env.download_dir("/work", down)
    assert hashlib.sha256((down / "sub" / "big.bin").read_bytes()).digest() == hashlib.sha256(blob).digest()
    assert os.readlink(down / "link") == "sub/big.bin" and (down / "run.sh").read_text() == "echo ran\n"
    await env.upload_file(local / "run.sh", "/app/copy.sh")
    await env.download_file("/app/copy.sh", scratch / "copy.sh")
    assert (scratch / "copy.sh").read_text() == "echo ran\n"
    try:
        await env.download_file("/nope", scratch / "nope")
        raise AssertionError("a missing file")
    except FileNotFoundError:
        pass
    await env.stop(delete=True)


def check_openenv(base, task_id):
    client = Client(base, os.environ["SELFDB_ENV_API_KEY"])
    spec = client.task(task_id)
    spec["instruction"] = "Make /app/calc.sh print 5."
    task = client.create_task(spec)
    env = OpenEnv(task, base_url=base, max_steps=3)
    obs = env.reset(seed=4)
    assert obs.output == "Make /app/calc.sh print 5." and not obs.done, vars(obs)
    assert env.state.step_count == 0 and env.state.seed == 4
    obs = env.step(SelfdbAction(tool="edit_file", path="/app/calc.sh", old="2 - 3", new="2 + 3"))
    assert obs.exit_code == 0 and obs.changes["modified"] == ["/app/calc.sh"] and not obs.done, vars(obs)
    obs = env.step(SelfdbAction(tool="bash", command="echo $SEED"))
    assert obs.output == "4\n"
    obs = env.step(SelfdbAction(tool="submit"))
    assert obs.done and obs.reward == 1.0, vars(obs)
    # out of steps: judged where it stands
    env.reset(seed=1)
    for i in range(3):
        obs = env.step(SelfdbAction(tool="bash", command="true"))
    assert obs.done and obs.reward == 0.0 and env.state.step_count == 3, vars(obs)
    try:
        env.step(SelfdbAction(tool="rm_rf"))
        raise AssertionError("an unknown tool")
    except ValueError:
        pass
    env.close()


def main(base, task_id, scratch):
    asyncio.run(check_harbor(base, Path(scratch)))
    check_openenv(base, task_id)
    print("ok")


if __name__ == "__main__":
    main(*sys.argv[1:4])
