from enum import Enum


class NetworkMode(str, Enum):
    NO_NETWORK = "no-network"
    PUBLIC = "public"
    ALLOWLIST = "allowlist"


class NetworkPolicy:
    def __init__(self, network_mode=NetworkMode.PUBLIC, allowed_hosts=None):
        self.network_mode = network_mode
        self.allowed_hosts = list(allowed_hosts or [])


class EnvironmentConfig:
    def __init__(self, docker_image=None, workdir=None, env=None):
        self.docker_image = docker_image
        self.workdir = workdir
        self.env = dict(env or {})
