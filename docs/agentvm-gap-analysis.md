# 距离完整 AgentVM 还差什么

基于当前 HEAD（`9da4c90` 之后）的代码与实测，分三个层面：

- **A. AgentVM 自身能用吗**：能。单步执行、快照回滚、变更清单、策略保护都已闭环并测过。
- **B. Agent 宿主能用吗**：不能。缺的是"跑一个 agent"所需的东西，不是"存一个 VM"所需的东西。
- **C. 生产可用吗**：还差并发与资源限制两类硬缺口。

判断依据是代码现状 + 实测，不是计划文档。每条缺口后面标了证据。

---

## 一、已具备（不需要再做）

| 能力 | 状态 |
|---|---|
| 单文件 VM、导入、增量同步、深度校验 | 完整，测试齐全 |
| 内容级快照、瞬间回滚、精确 diff | 完整 |
| 块级随机写（64 KiB，COW 到 chunk 粒度） | 完整，2616ms → 66ms |
| Base/Delta 叠加层（只读 Base + 可写 Delta） | 完整，含多进程与读一致性测试 |
| 写入策略（触发器级，含叠加层与 UPDATE 双向检查） | 完整 |
| 工具注入（跨库合并，字典重映射） | 完整 |
| 挂载所有者模型、链接不外逃 | 完整 |
| 单步执行（隔离、超时、输出上限、回滚、失败可见） | 完整，含真实挂载写入测试 |

---

## 二、B 层缺失：跑不起来一个 agent

### 1. 没有 agent 循环（最大的缺失）

`src/` 里没有任何 LLM 调用、工具选择、多轮对话。`agent-step` 只是"执行一条命令并记账"的原语。

**这是定位问题，不是 bug。** 现在的代码是**执行底座**，不是 agent。完整 AgentVM 需要上层：

- 一个循环：模型输出 → 解析工具调用 → `run_step` → 把结果回灌模型；
- 上下文管理：每步的 stdout 进上下文，超出预算需要压缩或摘要；
- 多步轨迹：`agent_steps` 目前**没有 `parent_step_id`**（已核对），无法表达分支/重试的树形结构。

**证据**：`grep -rn "http\|reqwest\|llm" src/` 无结果；`agent_steps` 无 `parent_step_id`。

### 2. 并发执行同一 VM 会撞车（实测）

同一 DB 上同时跑两个 step，第二个直接失败：

```
Error: UNIQUE constraint failed: vm_snapshots.name
```

原因：`next_snapshot_name` 用 `SELECT COALESCE(MAX(id),0)+1` 在**事务外**算名字，两个进程算到同一个名字。

**为什么重要**：并行工具调用（一次模型响应里多个 tool call）是常见需求。现在必须串行，否则丢步骤。

**修法**：名字由数据库分配（例如用 `vm_snapshots` 的自增 id，或插入后回填），而不是先查后插。

### 3. 没有资源限制

`run_step` 只有 `timeout_secs` 和 `max_output_bytes`（已核对 `StepOptions`）。没有：

- 内存上限（cgroup 或 `RLIMIT_AS`）——一个 `malloc` 炸弹能让宿主 OOM；
- CPU 上限（只有墙钟超时，不限制核数）；
- 磁盘写量上限——VM 数据库会一直长，`vm-step` 无任何约束。

**为什么重要**：不可信代码的隔离不能只靠命名空间。bwrap 管命名空间，不管资源。

**证据**：`grep -rn "cgroup\|setrlimit\|RLIMIT" src/` 无结果。

### 4. `vm-chroot` 与 `agent-step` 隔离不一致（已实测）

`vm-chroot` 的 FUSE 路径**没有 `--unshare-net`**，沙箱内可见宿主全部五个网卡。已在 `docs/vm.md` 记录，代码未改。

**为什么重要**：两条路径都能执行代码，但安全级别不同。用错一条就没有隔离。

---

## 三、C 层缺失：生产可用

### 5. 挂载丢失符号链接下的路径（P0.1 的硬阻塞）

实测：`vm-import /bin/sh` 的库，挂载里 `/bin/sh` **完全不存在**（`find`、`ls` 都看不到），`bwrap --bind <mount> /` 无法运行 `/bin/sh`。而 `vm-materialize` 有对应处理（写到 `/usr/bin/sh`）。

**影响**：FUSE 挂载不能作为 bwrap 根，`run_step` 因此必须走"全量物化 + 扫盘同步"。这是 P0.1 想优化掉的两步 I/O。

### 6. Base/Delta 未接通执行链

`agent.rs` 与 `fuse.rs` 都不引用 `Overlay`（已核对）。所以：

- `run_step` 只能操作单文件 DB，不能用"共享只读 Base + 每步一个 Delta"；
- 无法做"同一环境并行多个 agent，各自一个 Delta"；
- Delta 内的快照/回滚不含 `vm_whiteouts`（已核对 schema），分支回滚会把已删除的 Base 路径重新暴露。

**这是"Base+Delta 分支"从 API 到可用产品的最后一段**。

### 7. 硬链接被转成符号链接，导致数据丢失语义（已实测）

导入 tar 时 `is_hard_link()` 的条目直接存成符号链接（`vm.rs:1713`），与目标是两个独立条目，
不共享内容。

实测（`a.txt` 与 `b.txt` 是同一 inode 的硬链接）：

```
VM 内：rm b.txt 之后 cat a.txt  →  No such file（数据没了）
宿主：rm b.txt 之后 cat a.txt  →  HLDATA（正确）
```

即：硬链接的"两个名字指向同一数据"语义丢失，删掉任一名字就丢掉另一份。

影响面：`git`（对象库大量使用硬链接）、`pnpm`、部分编译工具链的缓存；
一旦其中一步删除一个名字，其余名字读到的是不存在的文件。

修法二选一：导入时把硬链接记录为"指向同一内容引用"（共享 `vm_file_chunks`，
`vm_fs` 两行同 hash），或显式声明不支持并在文档中禁止依赖硬链接的工作流。

---

## 四、可靠性缺口

### 8. 崩溃后可用，但留孤儿快照（已实测）

`run_step` 的流程是**多个独立事务**：快照 → 物化 → 运行 → 同步 → diff → 记录 `agent_steps`。
实测：step 运行 2.5 秒时 `kill -9`，结果：

```
snapshots=1  steps=0      # 留下 step-before-1，没有 agent_steps 行
/a.txt 未入库              # kill 在 sync 之前，写入未保留（合理）
vm_verify --deep = ok      # 数据库本身完好
```

所以**数据不会损坏**，但会积累孤儿快照（`step-before-N` / `step-after-N` 无对应 step 行）。
`snapshot_retain(conn, keep)` 能回收，但 `run_step` 不自动调用。

风险是长期运行下快照表与快照 chunk 引用不断增长，占空间。修法：`run_step` 结束时按策略
自动 retain，或启动时清理无 `agent_steps` 引用的快照。

已固化为 `tests/agent_crash.rs`。

---

## 四之二、经实测排除的猜测

这两条我原先怀疑是缺口，实测后推翻，记下来避免以后重复怀疑：

### 排除一：同一 Base 多 Delta

原先我判断这一项「未验证」，实测后推翻。

- 顺序：两个 Delta 打开同一只读 Base、写同一路径，各自读到自己的版本（`ONE`/`TWO`），
  Base 仍为 `BASE`。
- 并发：4 个线程、每个 Delta 写 40 次，**160 次写入全部成功**，各自最终值是自己的最后一次写入，
  Base 始终为 `BASE`。

已固化为 `tests/overlay_shared_base.rs`。所以「并行多个 agent 共用一个只读环境」在存储层是成立的。

### 排除二：输出上限的开销

原先我猜测「子进程持续极速输出会让读线程忙等占满 CPU」，实测推翻：

```
--max-output 1024, dd if=/dev/zero：
  200 MiB  → 98 ms
  2000 MiB → 592 ms      （约 3.4 GB/s 的丢弃速率）
truncated 标记正确，exit=0
```

丢弃是线性成本且远低于磁盘/网络，不是瓶颈。此项从缺口清单移除。


---

## 五、建议顺序

按"是否阻塞 agent 宿主"排序：

1. **快照名竞态**（第二节 2）——小改动，解开并行工具调用。先做。
2. **资源限制**（第二节 3）——cgroup v2 或 `setrlimit`，安全必需。
3. **`vm-chroot` 隔离统一**（第二节 4）——一行参数，但影响行为，需你确认。
4. **挂载路径可达性 + Overlay 接入执行链**（第三节 5、6）——这是 P0.1/P0.2 的主体，收益是省掉每步全量 I/O。
5. **agent 循环**（第二节 1）——最大工作量，但不是本仓库现有设计的延伸，属于新层。需要先定接口：模型协议、工具描述格式、轨迹存储。

第 5 项之前，1–4 都是让底座变成"可安全并发承载 agent 的底座"。

---

## 六、需要你决定

1. **agent 循环是否属于本项目**？如果属于，需要先定协议（是否 OpenAI 兼容工具调用格式）。
2. **资源限制用 cgroup v2 还是 `setrlimit`**？前者更完整但需要权限，后者无需特权但只管单进程。
3. **`vm-chroot` 是否统一到 `agent-step` 的隔离参数**？
