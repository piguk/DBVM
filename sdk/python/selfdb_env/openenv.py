"""An OpenEnv environment backed by the SQL-VM environment service.

OpenEnv (https://github.com/meta-pytorch/OpenEnv) serves environments over HTTP/WebSocket with ``reset`` /
``step`` / ``state``; trainers that speak OpenEnv (TRL among them) can then drive rollouts of a task here:

    # serve.py, run with: SELFDB_ENV_API_KEY=... uvicorn serve:app --port 8000
    from selfdb_env.openenv import create_app
    app = create_app(task_id="n1-task-...", base_url="http://env-node:8650", max_steps=50)

Each OpenEnv session is one rollout of the task: ``reset(seed=...)`` starts it (the observation carries the
task's ``instruction``), ``step`` runs an action (the service's tools: bash, read_file, write_file, edit_file,
list_dir), and the ``submit`` tool, or reaching ``max_steps``, judges the rollout and ends the episode with its
reward. The API key comes from ``SELFDB_ENV_API_KEY``.
"""

import os
from typing import Any, Dict, List, Optional

from openenv.core.env_server.interfaces import Environment
from openenv.core.env_server.types import Action, Observation, State

from . import Client

__all__ = ["SelfdbAction", "SelfdbObservation", "SelfdbState", "SelfdbEnvironment", "create_app"]

TOOLS = ("bash", "read_file", "write_file", "edit_file", "list_dir", "submit")
# the fields each tool takes, besides "tool"
_FIELDS = {
    "bash": ("command", "timeout_secs", "cwd"),
    "read_file": ("path", "offset", "limit", "encoding"),
    "write_file": ("path", "content", "mode", "encoding"),
    "edit_file": ("path", "old", "new"),
    "list_dir": ("path",),
    "submit": (),
}


class SelfdbAction(Action):
    """One tool call. ``submit`` ends the episode and judges it."""
    tool: str
    command: Optional[str] = None
    path: Optional[str] = None
    content: Optional[str] = None
    old: Optional[str] = None
    new: Optional[str] = None
    offset: Optional[int] = None
    limit: Optional[int] = None
    mode: Optional[int] = None
    encoding: Optional[str] = None
    timeout_secs: Optional[int] = None
    cwd: Optional[str] = None


class SelfdbObservation(Observation):
    """What an action returned (the service's response); after the episode ends, ``reward`` is the judge's."""
    output: str = ""
    stderr: str = ""
    exit_code: int = 0
    timed_out: bool = False
    changes: Optional[Dict[str, List[str]]] = None
    entries: Optional[List[Dict[str, Any]]] = None


class SelfdbState(State):
    session_id: Optional[str] = None
    task_id: Optional[str] = None
    seed: Optional[int] = None


class SelfdbEnvironment(Environment):
    # each instance is its own rollout on the service
    SUPPORTS_CONCURRENT_SESSIONS = True

    def __init__(self, task_id, base_url=None, max_steps=None, label="openenv"):
        super().__init__()
        url = base_url or os.environ.get("SELFDB_ENV_URL")
        key = os.environ.get("SELFDB_ENV_API_KEY")
        if not url or not key:
            raise RuntimeError("set base_url (or SELFDB_ENV_URL) and SELFDB_ENV_API_KEY")
        self._client = Client(url, key)
        self._task_id = task_id
        self._max_steps = max_steps
        self._label = label
        self._session = None
        self._state = SelfdbState(task_id=task_id)

    def reset(self, seed=None, episode_id=None, **kwargs):
        self.close()
        self._session = self._client.session(task_id=self._task_id, label=self._label, seed=seed)
        self._state = SelfdbState(episode_id=episode_id or self._session.id, step_count=0, session_id=self._session.id,
                                  task_id=self._task_id, seed=seed)
        instruction = self._client.task(self._task_id).get("instruction") or ""
        return SelfdbObservation(output=instruction, metadata={"session_id": self._session.id})

    def _judge(self, obs_fields):
        verdict = self._session.judge()
        self._state.step_count += 1
        return SelfdbObservation(done=True, reward=verdict.get("reward"), metadata={"judge": verdict.get("details")}, **obs_fields)

    def step(self, action, timeout_s=None, **kwargs):
        if self._session is None:
            raise RuntimeError("call reset() first")
        if action.tool not in TOOLS:
            raise ValueError("unknown tool %r; one of %s" % (action.tool, ", ".join(TOOLS)))
        if action.tool == "submit":
            return self._judge({})
        call = {"tool": action.tool}
        for f in _FIELDS[action.tool]:
            v = getattr(action, f)
            if v is not None:
                call[f] = v
        if action.tool == "bash" and "timeout_secs" not in call and timeout_s is not None:
            call["timeout_secs"] = max(1, int(timeout_s))
        r = self._session.act(call)
        self._state.step_count += 1
        fields = {"output": r["output"], "stderr": r["stderr"], "exit_code": r["exit_code"], "timed_out": r["timed_out"],
                  "changes": r["changes"], "entries": r.get("entries")}
        if self._max_steps is not None and self._state.step_count >= self._max_steps:
            # out of steps: the episode ends where it is, judged as it stands
            self._state.step_count -= 1
            return self._judge(fields)
        return SelfdbObservation(**fields)

    @property
    def state(self):
        return self._state

    def close(self):
        if self._session is not None:
            self._session.close()
            self._session = None


def create_app(task_id, base_url=None, max_steps=None, **kwargs):
    """The OpenEnv HTTP/WebSocket app serving rollouts of ``task_id`` (see the module docs)."""
    from openenv.core.env_server.http_server import create_app as openenv_app
    return openenv_app(lambda: SelfdbEnvironment(task_id, base_url=base_url, max_steps=max_steps),
                       SelfdbAction, SelfdbObservation, env_name="selfdb", **kwargs)
