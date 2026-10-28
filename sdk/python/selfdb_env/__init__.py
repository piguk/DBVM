"""Client for the SQL-VM RL environment service (HTTP API v1).

Standard library only. ``Client`` and ``Session`` are synchronous; ``AsyncClient`` and ``AsyncSession``
offer the same calls as coroutines (each runs the synchronous call in a worker thread), for training loops
that drive many rollouts with asyncio.

    client = Client("http://env-node:8650", api_key)
    task = client.create_task({"template_id": tpl, "readonly": ["/tests"],
                               "judge": {"cmd": ["/bin/sh", "/tests/judge.sh"], "sees": ["/repo"]}})
    for s in client.rollouts(task, [{"label": "a", "seed": 1}, {"label": "b", "seed": 2}]):
        with s:
            s.bash("cd /repo && make test")
            print(s.judge()["reward"])
"""

import asyncio
import json
import urllib.error
import urllib.request

__all__ = ["Client", "Session", "Env", "AsyncClient", "AsyncSession", "EnvError"]


class EnvError(Exception):
    """An error answer from the service: ``status`` is the HTTP status, ``message`` its explanation."""

    def __init__(self, status, message):
        super().__init__("%s: %s" % (status, message))
        self.status = status
        self.message = message


class Client:
    def __init__(self, base_url, api_key, timeout=3700):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.timeout = timeout

    def _request(self, method, path, body=None, raw=False):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base_url + path, data=data, method=method)
        req.add_header("Authorization", "Bearer " + self.api_key)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                payload = resp.read()
        except urllib.error.HTTPError as e:
            payload = e.read()
            try:
                message = json.loads(payload).get("error", payload.decode(errors="replace"))
            except ValueError:
                message = payload.decode(errors="replace")
            raise EnvError(e.code, message) from None
        return payload if raw else json.loads(payload or b"null")

    # templates and tasks

    def templates(self):
        return self._request("GET", "/v1/templates")

    def delete_template(self, template_id):
        self._request("DELETE", "/v1/templates/" + template_id)

    def create_task(self, spec):
        """Register a task (see docs/env.md for the fields); returns its id."""
        return self._request("POST", "/v1/tasks", spec)["task_id"]

    def task(self, task_id):
        return self._request("GET", "/v1/tasks/" + task_id)

    def delete_task(self, task_id):
        """Delete a task no running session and no checkpoint uses."""
        self._request("DELETE", "/v1/tasks/" + task_id)

    # sessions

    def session(self, template_id=None, task_id=None, egress=None, label=None, seed=None):
        """A build session over a template (``egress`` lists the hosts it may reach), or a rollout of a task."""
        body = {"template_id": template_id, "task_id": task_id, "egress": egress or [], "label": label, "seed": seed}
        return Session(self, self._request("POST", "/v1/sessions", body)["session_id"])

    def rollouts(self, task_id, variants):
        """One rollout of the task per variant (``{"label": ..., "seed": ...}``), all from the same start."""
        ids = self._request("POST", "/v1/rollouts", {"task_id": task_id, "variants": variants})["session_ids"]
        return [Session(self, i) for i in ids]

    def act_batch(self, pairs):
        """One action in each of several sessions, run concurrently: ``[(session, action), ...]``. Returns the
        responses in order; a failed item is ``{"error": ...}``."""
        body = [{"session_id": getattr(s, "id", s), "action": a} for s, a in pairs]
        return self._request("POST", "/v1/actions", body)

    def usage(self):
        return self._request("GET", "/v1/usage")

    # checkpoints

    def fork_checkpoint(self, checkpoint_id, variants):
        """New sessions starting from a checkpoint (``Session.checkpoint``), one per variant; also after the
        checkpointed session has ended. A variant without a seed keeps that session's."""
        ids = self._request("POST", "/v1/checkpoints/%s/fork" % checkpoint_id, {"variants": variants})["session_ids"]
        return [Session(self, i) for i in ids]

    def checkpoints(self, session_id):
        """A session's checkpoints, oldest first (``id``, ``step``, ``label``, ``seed``, ``bytes``)."""
        return self._request("GET", "/v1/sessions/%s/checkpoints" % session_id)

    def delete_checkpoint(self, checkpoint_id):
        self._request("DELETE", "/v1/checkpoints/" + checkpoint_id)


class Session:
    """A running session. Every action returns the service's response: ``exit_code``, ``output``, ``stderr``,
    ``changes`` (created / modified / deleted paths) and ``duration_ms``."""

    def __init__(self, client, session_id):
        self.client = client
        self.id = session_id

    def __repr__(self):
        return "Session(%r)" % self.id

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def act(self, action):
        return self.client._request("POST", "/v1/sessions/%s/actions" % self.id, action)

    def bash(self, command, timeout_secs=None, cwd=None):
        return self.act({"tool": "bash", "command": command, "timeout_secs": timeout_secs, "cwd": cwd})

    def read_file(self, path, offset=0, limit=None):
        return self.act({"tool": "read_file", "path": path, "offset": offset, "limit": limit})

    def write_file(self, path, content, mode=None):
        return self.act({"tool": "write_file", "path": path, "content": content, "mode": mode})

    def edit_file(self, path, old, new):
        return self.act({"tool": "edit_file", "path": path, "old": old, "new": new})

    def list_dir(self, path):
        return self.act({"tool": "list_dir", "path": path})

    def fork(self, variants):
        """New sessions starting from this one's current state; this one keeps running."""
        ids = self.client._request("POST", "/v1/sessions/%s/fork" % self.id, {"variants": variants})["session_ids"]
        return [Session(self.client, i) for i in ids]

    def checkpoint(self):
        """Keep this session's files as of now, to start sessions from later (``Client.fork_checkpoint``).
        Returns the checkpoint (``id``, ``step``, ...). It counts against storage until deleted."""
        return self.client._request("POST", "/v1/sessions/%s/checkpoints" % self.id)

    def freeze(self, name):
        """Save the current state as a template; returns the template."""
        return self.client._request("POST", "/v1/sessions/%s/freeze" % self.id, {"name": name})

    def judge(self):
        """Score the session with its task's judge: ``reward`` (None if the judge failed) and ``details``."""
        return self.client._request("POST", "/v1/sessions/%s/judge" % self.id)

    def trajectory(self):
        raw = self.client._request("GET", "/v1/sessions/%s/trajectory" % self.id, raw=True)
        return [json.loads(line) for line in raw.decode().splitlines() if line.strip()]

    def close(self):
        try:
            self.client._request("DELETE", "/v1/sessions/" + self.id)
        except EnvError as e:
            if e.status != 404:
                raise


class Env:
    """One task as an environment: ``reset`` starts a fresh rollout, ``step`` runs an action."""

    def __init__(self, client, task_id, label=None):
        self.client = client
        self.task_id = task_id
        self.label = label
        self.session = None

    def reset(self, seed=None):
        self.close()
        self.session = self.client.session(task_id=self.task_id, label=self.label, seed=seed)
        return self.session

    def step(self, action):
        if self.session is None:
            raise RuntimeError("call reset() first")
        return self.session.act(action)

    def judge(self):
        return self.session.judge()

    def close(self):
        if self.session is not None:
            self.session.close()
            self.session = None


def _wrap(method):
    async def call(self, *args, **kwargs):
        return await asyncio.to_thread(method, self._sync, *args, **kwargs)
    call.__name__ = method.__name__
    call.__doc__ = method.__doc__
    return call


class AsyncSession:
    def __init__(self, session):
        self._sync = session
        self.id = session.id

    act = _wrap(Session.act)
    bash = _wrap(Session.bash)
    read_file = _wrap(Session.read_file)
    write_file = _wrap(Session.write_file)
    edit_file = _wrap(Session.edit_file)
    list_dir = _wrap(Session.list_dir)
    freeze = _wrap(Session.freeze)
    checkpoint = _wrap(Session.checkpoint)
    judge = _wrap(Session.judge)
    trajectory = _wrap(Session.trajectory)
    close = _wrap(Session.close)

    async def fork(self, variants):
        return [AsyncSession(s) for s in await asyncio.to_thread(self._sync.fork, variants)]

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        await self.close()


class AsyncClient:
    def __init__(self, base_url, api_key, timeout=3700):
        self._sync = Client(base_url, api_key, timeout)

    templates = _wrap(Client.templates)
    delete_template = _wrap(Client.delete_template)
    create_task = _wrap(Client.create_task)
    task = _wrap(Client.task)
    delete_task = _wrap(Client.delete_task)
    act_batch = _wrap(Client.act_batch)
    usage = _wrap(Client.usage)
    checkpoints = _wrap(Client.checkpoints)
    delete_checkpoint = _wrap(Client.delete_checkpoint)

    async def session(self, **kwargs):
        return AsyncSession(await asyncio.to_thread(self._sync.session, **kwargs))

    async def rollouts(self, task_id, variants):
        return [AsyncSession(s) for s in await asyncio.to_thread(self._sync.rollouts, task_id, variants)]

    async def fork_checkpoint(self, checkpoint_id, variants):
        return [AsyncSession(s) for s in await asyncio.to_thread(self._sync.fork_checkpoint, checkpoint_id, variants)]
