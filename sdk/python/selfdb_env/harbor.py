"""A Harbor environment backed by the SQL-VM environment service.

Harbor (https://github.com/harbor-framework/harbor) loads it by import path, in the job's environment config:

    [environment]
    import_path = "selfdb_env.harbor:SelfdbEnvironment"
    kwargs = { base_url = "http://env-node:8650", egress = ["pypi.org", "files.pythonhosted.org"] }

and the API key from the environment of the process running Harbor: ``SELFDB_ENV_API_KEY`` (``SELFDB_ENV_URL``
stands in for ``base_url``). Keys do not belong in job files.

Each trial is a build session over a template: ``kwargs.template`` (a template id or name), or else the task's
``docker_image`` as a template name. Import images beforehand (``self env-admin image NAME PATH --oci``); the
service does not build Dockerfiles.

Network: ``no-network`` gives none; ``allowlist`` gives the task's hosts (``*.example.com`` as ``.example.com``)
plus ``kwargs.egress``; ``public`` gives ``kwargs.egress`` only, as the service reaches listed hosts and nothing
else.

Commands run as root in the sandbox (its user namespace maps only root); a command for another user is an
error rather than silently run as root.
"""

import base64
import os
import re
import shlex
from pathlib import Path

from harbor.environments.base import BaseEnvironment, ExecResult
from harbor.environments.capabilities import EnvironmentCapabilities
from harbor.models.task.config import NetworkMode

from . import AsyncClient

__all__ = ["SelfdbEnvironment"]

_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
# bytes per read_file request (the service caps a read at a few MiB)
_CHUNK = 1 << 20
# one write_file request carries the whole file, base64 in a JSON body of at most 64 MiB
_MAX_UPLOAD = 40 << 20


class SelfdbEnvironment(BaseEnvironment):
    def __init__(self, *args, base_url=None, template=None, egress=None, **kwargs):
        url = base_url or os.environ.get("SELFDB_ENV_URL")
        key = os.environ.get("SELFDB_ENV_API_KEY")
        if not url or not key:
            raise RuntimeError("set kwargs.base_url (or SELFDB_ENV_URL) and SELFDB_ENV_API_KEY")
        self._client = AsyncClient(url, key)
        self._template = template
        self._egress = list(egress or [])
        self._session = None
        super().__init__(*args, **kwargs)

    @staticmethod
    def type():
        return "selfdb"

    @property
    def capabilities(self):
        return EnvironmentCapabilities(disable_internet=True, network_allowlist=True, network_allowlist_hostnames=True,
                                       network_allowlist_wildcard_hostnames=True, network_allowlist_ipv4_addresses=True)

    def _validate_definition(self):
        # nothing to build: the template comes from the service (see the module docs)
        pass

    async def _template_id(self):
        want = self._template or getattr(self.task_env_config, "docker_image", None)
        if not want:
            raise RuntimeError("no template: set kwargs.template or the task's docker_image")
        templates = await self._client.templates()
        for t in templates:
            if t["id"] == want:
                return t["id"]
        named = sorted((t for t in templates if t["name"] == want), key=lambda t: t["created_at"])
        if not named:
            raise RuntimeError("no template named %r; import the image with `self env-admin image %s PATH --oci`" % (want, want))
        # the newest import of that name
        return named[-1]["id"]

    def _hosts(self):
        policy = self.network_policy
        if policy.network_mode == NetworkMode.NO_NETWORK:
            return []
        hosts = list(self._egress)
        if policy.network_mode == NetworkMode.ALLOWLIST:
            hosts += ["." + h[2:] if h.startswith("*.") else h for h in policy.allowed_hosts]
        return hosts

    async def start(self, force_build=False):
        self._session = await self._client.session(template_id=await self._template_id(), egress=self._hosts())

    async def stop(self, delete=True):
        if self._session is not None:
            await self._session.close()
            self._session = None

    def _live(self):
        if self._session is None:
            raise RuntimeError("the environment is not started")
        return self._session

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        who = self._resolve_user(user)
        if who not in (None, 0, "0", "root"):
            raise RuntimeError("selfdb sandboxes run commands as root only, not as %r" % (who,))
        merged = self._merge_env(env) or {}
        cwd = cwd or getattr(self.task_env_config, "workdir", None)
        for name in merged:
            if not _NAME.match(name):
                raise ValueError("not an environment variable name: %r" % name)
        # the command as harbor's container providers run it: bash -c (sh where an image has no bash)
        script = "".join("export %s=%s\n" % (k, shlex.quote(v)) for k, v in merged.items())
        script += "__harbor_cmd=%s\n" % shlex.quote(command)
        script += 'if [ -x /bin/bash ]; then exec /bin/bash -c "$__harbor_cmd"; fi\nexec /bin/sh -c "$__harbor_cmd"\n'
        r = await self._live().bash(script, timeout_secs=timeout_sec, cwd=cwd)
        stderr = r["stderr"]
        if r["timed_out"]:
            stderr += "\n[selfdb] timed out after %ss" % timeout_sec
        return ExecResult(stdout=r["output"], stderr=stderr, return_code=124 if r["timed_out"] else r["exit_code"])

    async def _check(self, response, what):
        if response["exit_code"] != 0:
            raise RuntimeError("%s: %s" % (what, response["stderr"].strip()))
        return response

    async def upload_file(self, source_path, target_path):
        data = Path(source_path).read_bytes()
        if len(data) > _MAX_UPLOAD:
            raise RuntimeError("%s: %d bytes; uploads are limited to %d" % (source_path, len(data), _MAX_UPLOAD))
        mode = os.stat(source_path).st_mode & 0o7777
        await self._check(await self._live().act({"tool": "write_file", "path": target_path, "mode": mode,
                                                  "content": base64.b64encode(data).decode(), "encoding": "base64"}),
                          "upload %s" % target_path)

    async def upload_dir(self, source_dir, target_dir):
        source_dir = Path(source_dir)
        await self._check(await self._live().bash("mkdir -p %s" % shlex.quote(target_dir)), "mkdir %s" % target_dir)
        for root, dirs, files in os.walk(source_dir):
            rel = Path(root).relative_to(source_dir)
            dest = Path(target_dir) / rel
            for d in dirs:
                p = Path(root) / d
                if p.is_symlink():
                    await self._symlink(os.readlink(p), str(dest / d))
                else:
                    await self._check(await self._live().bash("mkdir -p %s" % shlex.quote(str(dest / d))), "mkdir %s" % (dest / d))
            for f in files:
                p = Path(root) / f
                if p.is_symlink():
                    await self._symlink(os.readlink(p), str(dest / f))
                else:
                    await self.upload_file(p, str(dest / f))

    async def _symlink(self, target, path):
        await self._check(await self._live().bash("ln -sfn %s %s" % (shlex.quote(target), shlex.quote(path))), "link %s" % path)

    async def download_file(self, source_path, target_path):
        out = bytearray()
        while True:
            r = await self._live().act({"tool": "read_file", "path": source_path, "offset": len(out), "limit": _CHUNK, "encoding": "base64"})
            if r["exit_code"] != 0:
                raise FileNotFoundError("%s: %s" % (source_path, r["stderr"].strip()))
            chunk = base64.b64decode(r["output"])
            out += chunk
            if len(chunk) < _CHUNK:
                break
        target = Path(target_path)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(bytes(out))

    async def download_dir(self, source_dir, target_dir):
        r = await self._live().list_dir(source_dir)
        if r["exit_code"] != 0:
            raise FileNotFoundError("%s: %s" % (source_dir, r["stderr"].strip()))
        Path(target_dir).mkdir(parents=True, exist_ok=True)
        for e in r.get("entries") or []:
            src, dest = source_dir.rstrip("/") + "/" + e["name"], Path(target_dir) / e["name"]
            if e["kind"] == "dir":
                await self.download_dir(src, dest)
            elif e["kind"] == "symlink":
                link = await self._check(await self._live().bash("readlink %s" % shlex.quote(src)), "readlink %s" % src)
                if dest.is_symlink() or dest.exists():
                    dest.unlink()
                os.symlink(link["output"].rstrip("\n"), dest)
            elif e["kind"] == "file":
                await self.download_file(src, dest)
