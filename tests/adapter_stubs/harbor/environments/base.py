from abc import ABC, abstractmethod

from harbor.models.task.config import NetworkPolicy


class ExecResult:
    def __init__(self, stdout=None, stderr=None, return_code=0):
        self.stdout, self.stderr, self.return_code = stdout, stderr, return_code


class BaseEnvironment(ABC):
    def __init__(self, environment_dir, environment_name, session_id, trial_paths, task_env_config, logger=None,
                 persistent_env=None, network_policy=None, **kwargs):
        self.environment_dir = environment_dir
        self.environment_name = environment_name
        self.session_id = session_id
        self.trial_paths = trial_paths
        self.task_env_config = task_env_config
        self._persistent_env = persistent_env or {}
        self._network_policy = network_policy or NetworkPolicy()
        self.default_user = None
        self._validate_definition()

    @property
    def network_policy(self):
        return self._network_policy

    def _merge_env(self, env):
        merged = {**self._persistent_env, **(env or {})}
        return merged or None

    def _resolve_user(self, user):
        return user if user is not None else self.default_user

    @staticmethod
    @abstractmethod
    def type(): ...

    @abstractmethod
    def _validate_definition(self): ...

    @abstractmethod
    async def start(self, force_build): ...

    @abstractmethod
    async def stop(self, delete): ...

    @abstractmethod
    async def upload_file(self, source_path, target_path): ...

    @abstractmethod
    async def upload_dir(self, source_dir, target_dir): ...

    @abstractmethod
    async def download_file(self, source_path, target_path): ...

    @abstractmethod
    async def download_dir(self, source_dir, target_dir): ...

    @abstractmethod
    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None): ...
