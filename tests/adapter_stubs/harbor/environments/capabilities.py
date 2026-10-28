class EnvironmentCapabilities:
    FIELDS = ("stream", "gpus", "tpus", "disable_internet", "network_allowlist", "network_allowlist_hostnames",
              "network_allowlist_wildcard_hostnames", "network_allowlist_ipv4_addresses", "network_allowlist_ipv6_addresses",
              "network_allowlist_ipv4_cidrs", "network_allowlist_ipv6_cidrs", "dynamic_network_policy", "windows", "mounted",
              "docker_compose")

    def __init__(self, **kw):
        for k in kw:
            if k not in self.FIELDS:
                raise TypeError("unknown capability %r" % k)
        for f in self.FIELDS:
            setattr(self, f, kw.get(f, False))
