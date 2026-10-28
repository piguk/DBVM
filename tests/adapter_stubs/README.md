Stand-ins for the parts of Harbor and OpenEnv the adapters in `sdk/python/selfdb_env/` use, with the same
module paths and signatures (Harbor `src/harbor/environments/base.py`, OpenEnv
`src/openenv/core/env_server/{interfaces,types}.py`), so `tests/env_adapters.rs` runs without installing either.
They are not the frameworks: a release that changes these interfaces needs the adapters checked against it.
