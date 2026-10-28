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

rollout 的根是 bwrap overlay：lower 为模板的各层，upper 为 rollout 私有目录。启动不需要物化；每个动作的变更清单由扫描 upper 得到，与模板大小无关；每步不写数据库，只有 freeze 时把 upper 应用进新模板的数据库（`vm_apply_upper`）。

## 部署

```sh
cargo build --release
self env-admin --data /srv/env image import debian /path/to/debian-rootfs.tar   # 系统模板
self env-admin --data /srv/env tenant lab-a --max-sessions 1000                  # 打印租户 id
self env-admin --data /srv/env key <tenant-id>                                   # 打印 API key，只显示一次
self env-serve --data /srv/env --listen 0.0.0.0:8650 --node n1
```

- 以 systemd 服务运行，`Delegate=yes`，并设置 `SELF_SANDBOX_CGROUP` 为服务自身的 cgroup：每个 sandbox 一个子 cgroup（见 `docs/vm.md`「安全边界」）。
- 数据目录布局见 `src/env/store.rs`；`run/` 为运行中 rollout 的 upper，服务重启时清空。
- `env-admin` 只打开控制库与存储，不影响运行中的服务。
- `env-serve` 启动时进入一个只映射自身 uid/gid 的 user namespace，保留 `CAP_DAC_OVERRIDE`、`CAP_DAC_READ_SEARCH`、`CAP_FOWNER`（只对本用户的文件有效）。原因：agent 在 sandbox 内是 root，可以把目录设为 000；没有这些权限，宿主侧的复制、diff 和磁盘配额统计会读不到这些目录。在该 namespace 中 setuid 程序不生效，因此 bwrap 不能依赖 setuid 安装。
- 节点 id（`--node`）是所有对象 id 的前缀，为多节点路由预留。

## 接口（`/v1`）

鉴权：`Authorization: Bearer <key>`。错误为 `{"error": "..."}`：401（无 key 或 key 无效）、404（不存在，或属于其他租户）、400（请求不合法）、429（超出配额）、500。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/v1/templates` | 系统模板与本租户的模板 |
| DELETE | `/v1/templates/{id}` | 删除本租户模板；有运行中的 session 在用时返回 400 |
| POST | `/v1/tasks` | 创建任务，返回 `task_id` |
| GET | `/v1/tasks/{id}` | 任务定义 |
| POST | `/v1/sessions` | `{"template_id", "egress": [...]}` 创建 build session；`{"task_id", "label", "seed"}` 创建 rollout |
| POST | `/v1/rollouts` | `{"task_id", "variants": [{"label", "seed"}, ...]}`：同一起点的多个 rollout |
| POST | `/v1/sessions/{id}/actions` | 执行一个动作 |
| POST | `/v1/actions` | `[{"session_id", "action"}, ...]`：多个 session 各执行一个动作，并发进行 |
| POST | `/v1/sessions/{id}/fork` | `{"variants": [...]}`：从当前文件系统状态分叉；父 session 继续运行（见下文「fork」） |
| POST | `/v1/sessions/{id}/freeze` | `{"name"}`：保存为本租户模板 |
| POST | `/v1/sessions/{id}/judge` | 判分 |
| GET | `/v1/sessions/{id}/trajectory` | 轨迹（JSON lines），session 结束后仍可获取 |
| DELETE | `/v1/sessions/{id}` | 结束 session |
| GET | `/v1/usage` | 本租户各 session 的运行时长、CPU 时间与步数 |

### 动作

```json
{"tool": "bash", "command": "make test", "timeout_secs": 120, "cwd": "/repo"}
{"tool": "read_file", "path": "/repo/a.py", "offset": 0, "limit": 65536}
{"tool": "write_file", "path": "/repo/a.py", "content": "...", "mode": 420}
{"tool": "edit_file", "path": "/repo/a.py", "old": "x = 1", "new": "x = 2"}
{"tool": "list_dir", "path": "/repo"}
```

返回：`step`、`exit_code`、`timed_out`、`output`、`stderr`、`output_truncated` / `stderr_truncated`（每路最多 1 MiB）、`lossy`（输出含非 UTF-8 字节已替换）、`changes`（`created` / `modified` / `deleted`）、`duration_ms`，`list_dir` 另有 `entries`。文件工具在 sandbox 内执行，只读路径对它们同样生效；`edit_file` 要求 `old` 恰好出现一次。

### 任务

```json
{
  "template_id": "n1-tpl-...",
  "readonly": ["/tests"],
  "judge": {"cmd": ["/bin/sh", "/tests/judge.sh"], "timeout_secs": 300, "sees": ["/repo"]},
  "limits": {"memory_mb": 4096, "pids": 1024, "cpu_percent": 200, "file_size_mb": 1024},
  "step_timeout_secs": 120,
  "env": {"PYTHONPATH": "/repo"},
  "seed_env": "SEED"
}
```

- `readonly`：rollout 开始前以只读方式挂载（模板中的原内容）；模板中不存在的路径挂载为空的只读目录，agent 也无法创建。
- `judge`：在新的 sandbox 中运行，根为原始模板加上 rollout 在 `sees` 路径下的改动，其他路径一律来自原始模板；agent 改动 `/usr` 或留下的后台进程影响不到判分。`sees` 路径下的内容会被判分命令读取甚至执行（例如 `/repo/conftest.py`），任务设计需要考虑这一点：隔离只排除环境污染，判分规则本身的漏洞仍需任务作者处理。命令输出一个 JSON 对象，含数值 `reward`（整个 stdout，或最后一个非空行）；退出码非 0、超时或没有 `reward` 时 `reward` 为 null 并给出 `error`。
- 可复现：rollout 的 seed 写入 `seed_env` 与 `PYTHONHASHSEED`；固定 `TZ=UTC`、`LANG=LC_ALL=C.UTF-8`、hostname `vm`；独立 PID namespace。系统时钟不虚拟化。
- 不带 `judge` 时，训练方可根据轨迹与 diff 自行计算奖励。
- `limits` 每项都有上限（例如内存 32 GiB），0 不被接受为"不限"。

### fork

- 复制的是文件系统状态。进程、内存、打开的文件和 socket 不复制：子 rollout 从父 rollout 的文件重新开始，父 rollout 的后台服务需要在子 rollout 中重新启动。
- 复制期间父 rollout 的所有进程被冻结（cgroup v2 freezer），复制结果是某一时刻的完整状态，不会混入复制过程中新写的文件。冻结不保证应用自身的一致性：例如某个进程正处在两次写入之间，这个中间状态会被原样复制。需要应用级一致性时，应在 fork 前让应用到达安全点。
- 只能从当前状态分叉。要从轨迹中较早的某一步分叉，需要在那一步时做 fork 并保留该子 session。
- 每次 fork 请求复制一次父 rollout 的 upper（自它启动以来写入的内容），同一请求中的多个子 rollout 共用这一层。

### 出站

只有 build session 可以联网，且只能访问 `egress` 列出的主机：`host`（80、443 端口）、`host:port`、`.domain`（该域名及其子域名）。sandbox 内设置了 `HTTP_PROXY` / `HTTPS_PROXY`，指向 supervisor 的转发端口，流量经宿主侧代理（`src/egress.rs`）按白名单放行，其余返回 403 并记录。rollout 一律断网。

## 配额

| 配额 | 默认 |
|---|---|
| 同时运行的 session 数 | 256 |
| 存储（模板数据库与层） | 50 GiB |
| 单个 session 的最长运行时间 | 4 小时（超时由服务关闭） |
| 单个 session 写入的总字节数 | 4 GiB（超出时该步失败并关闭 session） |

## Python SDK

`sdk/python/selfdb_env`，只依赖标准库：`Client` / `Session`（同步），`AsyncClient` / `AsyncSession`（asyncio），`Env`（`reset` / `step` / `judge` / `close`）。示例：`sdk/python/examples/rollouts.py`。

## 实测（本机：16 核、27 GB；委托 cgroup；`tests/env_perf.rs`）

| 指标 | 结果 |
|---|---|
| 启动一个 rollout | p50 20.8 ms，p95 26.1 ms |
| fork 一个子 rollout（含冻结父 rollout） | p50 18.2 ms，p95 24.9 ms |
| 空动作（服务内 / 经 HTTP） | p50 1.36 ms / 1.53 ms |
| 500 个空闲 rollout | 依次启动共 11.4 s；每个 0.84–1.25 MB 内存（两次运行。整机 `MemTotal − MemAvailable` 的增量除以 500：含内核内存，不含可回收的 page cache，受宿主上其他进程影响） |
| 64 个 session 并发、每个 20 次写读动作 | 6917 动作/秒 |

## 不在当前范围内

多节点调度与模板分发、gVisor 下的 overlay rollout（gVisor 的 overlay 以自有格式保存写入，宿主无法逐路径读取）、rollout 的逐步 undo 与从历史步骤分叉（以 fork 代替）、运行态（进程与内存）的 fork、从 Docker 镜像导入、计费与支付、虚拟时钟、PTY 与端口暴露。批量创建 rollout 目前依次启动。
