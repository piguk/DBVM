class _Model:
    """Fields from class annotations, defaults from class attributes (as pydantic declares them)."""
    EXTRA = False

    def __init__(self, **kw):
        fields = {}
        for klass in reversed(type(self).__mro__):
            fields.update(getattr(klass, "__annotations__", {}))
        for name in fields:
            if name in kw:
                setattr(self, name, kw.pop(name))
            elif hasattr(type(self), name):
                default = getattr(type(self), name)
                setattr(self, name, dict(default) if isinstance(default, dict) else default)
            else:
                raise TypeError("%s: missing field %r" % (type(self).__name__, name))
        if kw and not self.EXTRA:
            raise TypeError("%s: unknown fields %s" % (type(self).__name__, sorted(kw)))
        for k, v in kw.items():
            setattr(self, k, v)


class Action(_Model):
    metadata: dict = {}


class Observation(_Model):
    done: bool = False
    reward: object = None
    metadata: dict = {}


class State(_Model):
    EXTRA = True
    episode_id: object = None
    step_count: int = 0
