from abc import ABC, abstractmethod


class Environment(ABC):
    SUPPORTS_CONCURRENT_SESSIONS = False

    def __init__(self, transform=None, rubric=None):
        self.transform, self.rubric = transform, rubric

    @abstractmethod
    def reset(self, seed=None, episode_id=None, **kwargs): ...

    @abstractmethod
    def step(self, action, timeout_s=None, **kwargs): ...

    @property
    @abstractmethod
    def state(self): ...

    def close(self):
        pass
