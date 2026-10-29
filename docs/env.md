# RL 环境服务

把 SQL-VM 作为 RL 训练用的环境服务部署在自有或租用的节点上；客户只通过 HTTP 接口（或 Python SDK）使用：从模板分叉出大量 rollout，在其中执行动作、拿到逐步 diff、判分，并支持同一起点的多个变体。

## 概念

| 名称 | 含义 |
|---|---|
| 模板（template） | 不可变：一个 VM 数据库（`base.db`）加一组有序的只读层。系统模板由服务商导入，所有租户可用；租户模板由 freeze 产生，只有该租户可见 |
| 层（layer） | 只读目录，作为 overlay 的 lower 被多个 rollout 共享。第一层是物化后的树，之后每层是一次 freeze 或 fork 冻结下来的 upper；fork 链超过 16 层时，子 rollout 的 fork 层合并为一层 |
| build session | 基于模板、用于定制环境的 session；可以开放出站白名单（pip、npm、git） |
| rollout | 基于任务的 session；无网络，只读路径由 mount 强制 |
| 任务（task） | 模板 + 只读路径 + 判分命令 + 资源限制 + 种子变量 |

rollout 的根是 bwrap overlay：lower 为模板的各层，upper 为 rollout 私有目录。启动不需要物化；每个动作的变更清单只检查 inotify 报告的 upper 路径，代价与这一步的改动量成正比，与模板大小和累计改动量无关（inotify 不确定时，以及每 64 步一次，改为遍历整个 upper）；每步不写数据库，只有 freeze 时把 upper 应用进新模板的数据库（`vm_apply_upper`）。

## 部署

```sh
cargo build --release
self env-admin --data /srv/env image python /path/to/image.tar --oci             # 系统模板：OCI 镜像
self env-admin --data /srv/env image debian /path/to/debian-rootfs.tar           # 或 rootfs tarball
self env-admin --data /srv/env tenant lab-a --max-sessions 1000                  # 打印租户 id
self env-admin --data /srv/env key <tenant-id>                                   # 打印 API key，只显示一次
self env-serve --data /srv/env --listen 0.0.0.0:8650 --node n1
```

- 安装包、systemd unit、Kubernetes 部署与未验证事项见 `packaging/README.md`。部署前运行 `self env-doctor --data DIR` 检查宿主条件。
- 以 systemd 服务运行，`Delegate=yes`，并设置 `SELF_SANDBOX_CGROUP=auto`（使用服务自身的 cgroup；也可写明路径）：每个 sandbox 一个子 cgroup（见 `docs/vm.md`「安全边界」）。
- 数据目录布局见 `src/env/store.rs`；`run/` 为运行中 rollout 的 upper，服务重启时清空。
- `env-admin` 只打开控制库与存储，不影响运行中的服务。
- `image --oci` 接受 OCI image layout（`skopeo copy docker://python:3.12-alpine oci:DIR`、`docker buildx build --output type=oci`）或 `docker save` 的输出，目录或 tar 文件均可。多架构镜像取 linux/本机架构。各层按顺序应用（含 whiteout 与 opaque 目录）；每层解压后的内容按 config 的 `rootfs.diff_ids` 校验，image layout 中的每个 blob 按其 digest 校验，不符则拒绝导入。config 的 `Env` 与 `WorkingDir` 成为模板的环境变量与默认工作目录（任务的 `env` 覆盖镜像的）；`Entrypoint`、`Cmd`、`User` 不使用（命令以 sandbox 内的 root 执行）。不从 registry 拉取：先用 skopeo 或 docker 取到本地。
- `env-serve` 启动时进入一个只映射自身 uid/gid 的 user namespace，保留 `CAP_DAC_OVERRIDE`、`CAP_DAC_READ_SEARCH`、`CAP_FOWNER`（只对本用户的文件有效）。原因：agent 在 sandbox 内是 root，可以把目录设为 000；没有这些权限，宿主侧的复制、diff 和磁盘配额统计会读不到这些目录。在该 namespace 中 setuid 程序不生效，因此 bwrap 不能依赖 setuid 安装。
- 所有 rollout 共用进程内的一个 inotify 实例；每个 rollout 的 upper 中每个目录占用一个 watch。`fs.inotify.max_user_watches` 用尽后，无法再添加 watch 的 rollout 改为每步遍历 upper（结果不变，变慢），需要时调大该值。
- 节点 id（`--node`）是所有对象 id 的前缀，多节点时据此路由。
- `--max-sessions N`：本节点同时运行的 session 上限（所有租户合计），超出返回 503。
- `--max-memory-mb`、`--max-pids`、`--max-cpu-percent`、`--max-file-size-mb`、`--max-action-secs`：任务可申请的资源上限（默认 32768、8192、1600、16384、3600）；任务申请超出时取上限。
- `--metrics-listen ADDR`：在 `http://ADDR/metrics` 提供 Prometheus 指标（动作与 session 启动耗时分布、各类操作计数、session 结束原因、运行中的 session、存储与层），不鉴权，应只监听内网地址。
- `self env-admin --data DIR support-bundle out.tar.gz`：诊断包，含宿主检查、版本、数据目录大小与控制库摘要，不含 API key、token、名称、label 与任务内容。

## 多节点

```sh
# 每台机器相同的 cluster token 文件（只有属主可读）
self env-coordinator --data /srv/coord --listen 0.0.0.0:8650 --id c1 --token-file /etc/selfdb/cluster-token
self env-serve --data /srv/env --node n1 --listen 0.0.0.0:8650 --max-sessions 400 \
    --coordinator http://coord:8650 --advertise http://node1:8650 --token-file /etc/selfdb/cluster-token
self env-admin --data /srv/coord --node c1 tenant lab-a      # 租户与 API key 在 coordinator 上
self env-admin --data /srv/env image eda eda.tar --oci       # 镜像导入到任一节点
```

客户端只访问 coordinator，接口与单节点相同。

- coordinator 保存租户、API key 与任务；节点保存模板、层、session、checkpoint 与轨迹。模板列表来自各节点的报告。
- 带 session 或 checkpoint id 的请求转发到 id 前缀对应的节点。启动 session 的请求按节点空闲容量放置，已有该模板的节点优先；一批 rollout 可以分到多个节点，全部成功或全部不启动。fork 与从 checkpoint 分叉在其所在节点进行。
- 节点缺少模板时，从持有它的节点拉取数据库与缺少的层：传输格式确定（`src/env/transfer.rs`），按首次导出时记录的 sha256 校验，不符则拒绝；下载期间不持锁，中断留下的目录在节点启动时清除。
- 节点启动时注册，此后每 10 秒重复注册；coordinator 每 2 秒查询各节点状态，连续两次失败的节点不再分配新 session，发往它的请求返回 503。
- coordinator 与节点之间、节点与节点之间以 cluster token 认证；转发的请求带 `X-Selfdb-Tenant`，API key 不离开 coordinator。节点之间为明文 HTTP，应部署在内网或经 TLS 代理。
- 配额：同时运行的 session 数按"各节点上次报告的数量 + 此后启动的数量 + 正在启动的数量"计算，只会偏大，跨节点也不会超出；存储配额由各节点分别检查各自保存的内容，租户在多个节点上的总量可能超出。

## 接口（`/v1`）

鉴权：`Authorization: Bearer <key>`。错误为 `{"error": "..."}`：401（无 key 或 key 无效）、404（不存在，或属于其他租户）、400（请求不合法）、429（超出配额）、500。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/v1/templates` | 系统模板与本租户的模板 |
| DELETE | `/v1/templates/{id}` | 删除本租户模板；有运行中的 session 在用时返回 400 |
| POST | `/v1/tasks` | 创建任务，返回 `task_id` |
| GET | `/v1/tasks/{id}` | 任务定义 |
| DELETE | `/v1/tasks/{id}` | 删除任务 |
| POST | `/v1/sessions` | `{"template_id", "egress": [...], "limits": {...}}` 创建 build session（`limits` 同任务的写法，受部署方上限约束；编译、安装依赖时常需更多内存）；`{"task_id", "label", "seed"}` 创建 rollout（资源与出站来自任务） |
| POST | `/v1/rollouts` | `{"task_id", "variants": [{"label", "seed"}, ...]}`：同一起点的多个 rollout，并行启动；全部成功或全部不启动，返回顺序与 `variants` 一致 |
| POST | `/v1/sessions/{id}/actions` | 执行一个动作 |
| POST | `/v1/actions` | `[{"session_id", "action"}, ...]`：多个 session 各执行一个动作，并发进行 |
| POST | `/v1/sessions/{id}/fork` | `{"variants": [...]}`：从当前文件系统状态分叉；父 session 继续运行（见下文「fork」） |
| POST | `/v1/sessions/{id}/checkpoints` | 保存当前文件系统状态为 checkpoint，返回 `id`、`step` 等 |
| GET | `/v1/sessions/{id}/checkpoints` | 该 session 的 checkpoint，按步数排序；session 结束后仍可查询 |
| POST | `/v1/checkpoints/{id}/fork` | `{"variants": [...]}`：从 checkpoint 分叉；源 session 结束后仍可使用 |
| DELETE | `/v1/checkpoints/{id}` | 删除 checkpoint |
| POST | `/v1/sessions/{id}/freeze` | `{"name"}`：保存为本租户模板 |
| POST | `/v1/sessions/{id}/judge` | 判分 |
| GET | `/v1/sessions/{id}/trajectory` | 轨迹（JSON lines），session 结束后仍可获取 |
| DELETE | `/v1/sessions/{id}` | 结束 session |
| GET | `/v1/usage` | 本租户各 session 的运行时长、CPU 时间与步数 |

### 动作

```json
{"tool": "bash", "command": "make test", "timeout_secs": 120, "cwd": "/repo"}
{"tool": "read_file", "path": "/repo/a.py", "offset": 0, "limit": 65536}
{"tool": "read_file", "path": "/repo/data.bin", "encoding": "base64"}
{"tool": "write_file", "path": "/repo/a.py", "content": "...", "mode": 420}
{"tool": "write_file", "path": "/repo/data.bin", "content": "<base64>", "encoding": "base64"}
{"tool": "edit_file", "path": "/repo/a.py", "old": "x = 1", "new": "x = 2"}
{"tool": "list_dir", "path": "/repo"}
```

返回：`step`、`exit_code`、`timed_out`、`output`、`stderr`、`output_truncated` / `stderr_truncated`（每路最多 1 MiB）、`lossy`（输出含非 UTF-8 字节已替换）、`changes`（`created` / `modified` / `deleted`）、`duration_ms`，`list_dir` 另有 `entries`。文件工具在 sandbox 内执行，只读路径对它们同样生效；`edit_file` 要求 `old` 恰好出现一次。`encoding: "base64"` 用于任意字节（默认 `utf8`）。`write_file` 给出 `mode` 时文件权限即为该值（已存在的文件也是）；不给时新文件为 0644，已存在的文件保持原权限。

### 任务

```json
{
  "template_id": "n1-tpl-...",
  "instruction": "Make the tests in /tests pass.",
  "readonly": ["/tests"],
  "judge": {"cmd": ["/bin/sh", "/tests/judge.sh"], "timeout_secs": 300, "sees": ["/repo"]},
  "limits": {"memory_mb": 4096, "pids": 1024, "cpu_percent": 200, "file_size_mb": 1024},
  "step_timeout_secs": 120,
  "env": {"PYTHONPATH": "/repo"},
  "seed_env": "SEED",
  "files": [{"path": "/repo/tests/test_x.py", "content": "...", "mode": 420}, {"path": "/repo/data.bin", "content": "<base64>", "encoding": "base64"}],
  "egress": ["license.internal:27000"],
  "services": [{"name": "db", "run": "exec postgres -D /var/lib/pg", "ready": "pg_isready", "ready_timeout_secs": 30}]
}
```

判分可以带只有判分可见的文件：`"judge": {"cmd": [...], "sees": [...], "services": false, "files": [{"path": "/judge/expected.csv", "content": "..."}]}`。

- `files`：任务的文件（代码、数据、测试），创建任务时做成一个只读层，该任务的所有 rollout 共用；rollout 启动后它们已在原位，不计入 agent 的改动；判分时完整可见；freeze 时写入新模板。最多 32 MiB。这样同一个工具模板可以承载大量任务，每个任务只多一个小层。
- `judge.files`：只在判分 sandbox 中出现的文件（隐藏测试、期望结果），rollout 中看不到；位于判分 sandbox 的最上层，agent 在 `sees` 路径下写的同名文件不会覆盖它们。
- `judge.services`：为 true 时，判分 sandbox 先启动任务的服务（例如查询 rollout 留下的数据库状态）。
- `egress`：该任务的 rollout 可以访问的主机（写法同出站白名单），例如 license server、内部 API；不写则无网络。
- `services`：与 agent 并行运行的进程（数据库、内部 API 的替身）：每个 rollout（含 fork 与从 checkpoint 分叉出的）在第一个动作之前启动它们，并等待 `ready` 命令成功（超时则 rollout 启动失败，错误中附带服务日志末尾）；输出在 `/tmp/selfdb-services/<name>.log`；服务启动时写入的文件不计入 agent 的改动。fork 不复制进程，子 rollout 中服务重新启动。
- 删除任务（`DELETE /v1/tasks/{id}`）：有运行中的 session 或 checkpoint 使用时返回 400；被任务使用的模板不能删除。

- `readonly`：rollout 开始前以只读方式挂载（模板中的原内容）；模板中不存在的路径挂载为空的只读目录，agent 也无法创建。
- `judge`：在新的 sandbox 中运行，根为原始模板加上 rollout 在 `sees` 路径下的改动，其他路径一律来自原始模板；agent 改动 `/usr` 或留下的后台进程影响不到判分。`sees` 路径下的内容会被判分命令读取甚至执行（例如 `/repo/conftest.py`），任务设计需要考虑这一点：隔离只排除环境污染，判分规则本身的漏洞仍需任务作者处理。命令输出一个 JSON 对象，含数值 `reward`（整个 stdout，或最后一个非空行）；退出码非 0、超时或没有 `reward` 时 `reward` 为 null 并给出 `error`。
- 可复现：rollout 的 seed 写入 `seed_env` 与 `PYTHONHASHSEED`；固定 `TZ=UTC`、`LANG=LC_ALL=C.UTF-8`、hostname `vm`；独立 PID namespace。系统时钟不虚拟化。
- 不带 `judge` 时，训练方可根据轨迹与 diff 自行计算奖励。
- `limits` 每项都有上限（例如内存 32 GiB），0 不被接受为"不限"。

### fork

- 复制的是文件系统状态。进程、内存、打开的文件和 socket 不复制：子 rollout 从父 rollout 的文件重新开始，父 rollout 的后台服务需要在子 rollout 中重新启动。
- 复制期间父 rollout 的所有进程被冻结（cgroup v2 freezer），复制结果是某一时刻的完整状态，不会混入复制过程中新写的文件。冻结不保证应用自身的一致性：例如某个进程正处在两次写入之间，这个中间状态会被原样复制。需要应用级一致性时，应在 fork 前让应用到达安全点。
- `fork` 从当前状态分叉。要在之后从某一步重新开始，在那一步做 checkpoint（见下文）。
- 每次 fork 请求复制一次父 rollout 的 upper（自它启动以来写入的内容），同一请求中的多个子 rollout 共用这一层。

### checkpoint

- checkpoint 保存 session 在某一步的文件系统状态（与 fork 相同：复制时冻结，不含进程）。轨迹中记录一行 `{"step", "checkpoint"}`，便于与动作对齐。
- 之后可以从 checkpoint 分叉任意次，源 session 继续运行或已经结束都可以。子 session 沿用 checkpoint 的任务与模板，label 由各 variant 指定；variant 不给 seed 时沿用源 session 的 seed。
- checkpoint 的层计入租户的存储配额，删除 checkpoint 后才释放；被 checkpoint 引用的模板不能删除。
- 典型用法：每 k 步做一次 checkpoint，训练方根据判分或价值估计，选择从哪些步重新展开多个分支。

### 出站

只有 build session 可以联网，且只能访问 `egress` 列出的主机：`host`（80、443 端口）、`host:port`、`.domain`（该域名及其子域名）。sandbox 内设置了 `HTTP_PROXY` / `HTTPS_PROXY`，指向 supervisor 的转发端口，流量经宿主侧代理（`src/egress.rs`）按白名单放行，其余返回 403 并记录。rollout 一律断网。

## 配额

| 配额 | 默认 |
|---|---|
| 同时运行的 session 数（含正在启动的；并发请求同样受限） | 256 |
| 存储（模板数据库与层） | 50 GiB |
| 单个 session 的最长运行时间 | 4 小时（超时由服务关闭） |
| 单个 session 写入的总字节数 | 4 GiB（超出时该步失败并关闭 session） |

## 任务包

任务包用 `packs/packkit` 生成与检查（候选任务 → 起始状态与解答校验 → 作弊用例检查），见其 README。判分能否被绕过，用 `packkit audit`（不需要模型：平凡答案、测试框架 hook 文件、判分脚本与答案是否可见）和 `packkit redteam`（让模型尝试绕过判分，记录得分异常的轨迹）检查。

| 包 | 内容 |
|---|---|
| `packs/eda/` | Verilator（RTL 修复）与 ngspice（电路参数） |
| `packs/data/` | 公司数据上的 SQL / CSV 分析与报表，期望结果为判分专用文件 |
| `packs/ops/` | 日志排障、服务配置修复、经 API 操作有状态服务（任务服务） |
| `packs/repo/` | 由客户自己的仓库生成：变异（单点注入缺陷）与 git 历史（撤销过去的修复），以仓库已有的测试判分 |

## 个人使用

```sh
self env-local --image rootfs.tar --image-name python    # ~/.local/share/selfdb/local；首次运行生成租户与 key（local-key，0600）
PYTHONPATH=packs:sdk/python python3 -m packkit build packs/data http://127.0.0.1:8650 <template-id> --per-family 5
python3 sdk/python/examples/local_agent.py tasks.json --model http://127.0.0.1:8080/v1 --group 8 --out rollouts.jsonl
```

- `self env-local` 只监听本机地址；SDK 的 `Client.local()` 读取它的地址与 key。
- `local_agent.py`：用任何 OpenAI 兼容的本地推理服务（llama.cpp、vLLM、ollama）对每个任务采样一组 rollout，模型通过工具调用执行动作、调用 `submit` 后判分，输出含消息、reward 与组内相对 advantage 的 JSON lines，可直接用于 GRPO 一类的训练。

## 接入训练框架

`sdk/python/selfdb_env/` 下有两个 adapter，均从环境变量 `SELFDB_ENV_API_KEY` 读取 API key：

- **Harbor**（`selfdb_env.harbor:SelfdbEnvironment`）：在 Harbor 的 job 配置中以 `import_path` 指定，`kwargs` 给出 `base_url`、可选的 `template`（模板 id 或名称；默认用任务的 `docker_image` 作为模板名称）与 `egress`。每个 trial 是一个 build session。网络策略：`no-network` 无网络；`allowlist` 为任务列出的主机（`*.x` 转为 `.x`）加上 `egress`；`public` 只有 `egress`，因为服务只放行列出的主机。不构建 Dockerfile：镜像需先用 `env-admin image ... --oci` 导入。命令以 root 执行，要求其他用户的命令会报错。
- **OpenEnv**（`selfdb_env.openenv`）：`create_app(task_id, base_url, max_steps)` 返回 OpenEnv 的 HTTP/WebSocket app。每个 OpenEnv 会话是任务的一个 rollout：`reset(seed)` 启动 rollout，observation 中带任务的 `instruction`；`step` 执行一个工具调用；`submit` 或达到 `max_steps` 时判分并结束，`reward` 为判分结果。

验证：

- 已用 harbor 0.25.0 与 openenv-core 0.3.0 验证：`harbor run -e selfdb_env.harbor:SelfdbEnvironment` 以 oracle 与 nop agent 运行任务，判分在同一环境（`shared`）与独立环境（`separate`，agent 的产出以 artifact 传入）两种模式下 reward 均符合预期（`tests/env_harbor.rs`）；OpenEnv 经其 `create_app`（uvicorn）与 `GenericEnvClient` 完成 reset、step、submit（`tests/env_adapters.rs`）。运行这些测试需设置 `SELFDB_ADAPTER_PYTHON` 为装有两个框架的 Python。
- 未设置时，`tests/env_adapters.rs` 使用 `tests/adapter_stubs/` 中按框架源码签名编写的替身。框架发布新版本后，需要用真实框架重新运行。

## Python SDK

`sdk/python/selfdb_env`，只依赖标准库：`Client` / `Session`（同步），`AsyncClient` / `AsyncSession`（asyncio），`Env`（`reset` / `step` / `judge` / `close`）。示例：`sdk/python/examples/rollouts.py`（API key 从环境变量 `SELFDB_ENV_API_KEY` 读取，不放在命令行参数里）。

## 实测（本机：16 核、27 GB；委托 cgroup；`tests/env_perf.rs`）

| 指标 | 结果 |
|---|---|
| 启动一个 rollout | p50 21.0 ms，p95 27.7 ms |
| fork 一个子 rollout（含冻结父 rollout） | p50 18.2–22.9 ms（两次运行） |
| 空动作（服务内 / 经 HTTP） | p50 1.34 ms / 1.49 ms |
| 500 个空闲 rollout | 依次启动共 10.5 s；每个 sandbox cgroup 的 `memory.current` 约 800 KiB（anon 343 KiB，内核 444 KiB，page cache 0） |
| 一次请求批量启动 500 个 rollout（并行） | 共 2.0–2.3 s（每个 4.0–4.6 ms）；逐个启动共 11.5 s |
| 64 个 session 并发、每个 20 次写读动作 | 7103 动作/秒（机器空闲时）；宿主另有约 3 核负载时 6000–6400 |
| 长轨迹：5000 步，每步新增一个文件 | 第 1–100 步 p50 1.35 ms，第 4901–5000 步 p50 1.35 ms、mean 1.65 ms（每 64 步一次全量遍历）；改为增量之前，第 4901–5000 步 p50 为 21.3 ms |

## 不在当前范围内

coordinator 的高可用（单实例，重启后由节点重新注册恢复）、节点宕机后其上 session 的迁移、gVisor 下的 overlay rollout（gVisor 的 overlay 以自有格式保存写入，宿主无法逐路径读取）、rollout 的逐步 undo（以 checkpoint 代替）、运行态（进程与内存）的 fork、从 registry 直接拉取镜像、计费与支付、虚拟时钟、PTY 与端口暴露。
