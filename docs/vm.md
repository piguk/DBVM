# 单文件 VM

一个 `*.db` 即系统。SQLite 表分工：

- `vm_fs(path UNIQUE, kind, mode, uid, gid, mtime, size, link_target, hash, compressed, content)`：路径/权限/内容
- `vm_blobs(hash PK, content, compressed, raw_size, refcnt)`：内容去重，`compressed 0=raw 2=zstd 3=zstd+dict`
- `vm_dicts`：zstd 字典，追加写入、不覆盖（`vm-train-dict` 每次新增一行）。压缩块的 `vm_blobs.dict_id` 记录其所用字典，因此重训后旧块仍可读
- `vm_meta`：O(1) 计数器（文件数、逻辑字节、blob 字节），由触发器维护，`vm-verify --deep` 可逐项对账
- `vm_mem`：内存镜像；`vm_snapshots` / `vm_log`：checkpoint 与变更日志
- `vm_disk_blocks(block_id PK, content, compressed, raw_size)`：4K 稀疏分页块设备；`vm_meta.disk_size` 记录逻辑大小
- `vm_dirty(path)`：整文件 hash 尚未更新的文件。经挂载的写入（`write_range_deferred`）只改 chunk、size、mtime，路径记入此表；读取 `vm_fs.hash` 的操作（快照、diff、物化、sync、verify、ls/stat）先调用 `finalize_dirty` 补算，`vm_open` 与卸载时也会补算。只读挂接的 Base 或 vm-inject 包若有未补算的行会被拒绝
- 库 `application_id = 0x564D5351 ("VMSQ")`，`user_version = 12`（当前 schema）。旧库在打开时按序迁移，每步一个事务；历史版本见 `src/vm/mod.rs` 顶部注释。v8 起有快照表，v9 起有 `agent_steps`，v10 补截断列，v11 起有 `vm_policies`，v12 起有 `vm_dirty`

新库 `page_size 8192`, `auto_vacuum=INCREMENTAL`；`--vm-only` 跳过 SELF 专用表（`segments/symbols/self_blob/bundle_*` 等）。连接 PRAGMA：`journal_mode=WAL synchronous=NORMAL mmap_size=256M busy_timeout=5000 journal_size_limit=64M`。

## 导入 rootfs

```sh
cargo build --release --features fuse   # pure-rust fuser，无需 libfuse
curl -L -o /tmp/alpine.tar.gz https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
self vm-init /tmp/alpine.vm.db --force --vm-only
self vm-import-rootfs /tmp/alpine.vm.db /tmp/alpine.tar.gz --exclude /usr/share/doc --exclude /var/cache
self vm-train-dict /tmp/alpine.vm.db --max-size 16384   # 可选：小文件字典压缩
self vm-verify /tmp/alpine.vm.db                        # integrity=ok ...
self vm-status /tmp/alpine.vm.db
```

`vm-import-rootfs` 支持 `--whitelist <prefix>`（可多次）与 `--exclude <prefix>`；导入文件数 > 50 时自动训练字典。

## 运行

```sh
self vm-chroot /tmp/alpine.vm.db                              # 交互 shell
self vm-chroot /tmp/alpine.vm.db /bin/sh -c 'cat /etc/alpine-release; ls /'
self vm-chroot /tmp/alpine.vm.db --ephemeral /bin/sh -c 'echo tmp-only'   # 丢弃本次会话的全部改动
self vm-exec   /tmp/alpine.vm.db /bin/busybox -- --help                  # 单文件：root 只含该程序与库
self vm-mount  /tmp/alpine.vm.db /tmp/mnt                                 # FUSE 挂载（需 /dev/fuse，--features fuse）
```

`vm-chroot`、`vm-exec`、`vm-step`、`vm-agent` 都通过同一个 sandbox（`src/sandbox.rs`）运行 VM 内容，隔离参数、syscall 过滤、资源限制与环境变量一致，见「安全边界」§1。无法建立 sandbox 时拒绝运行，不会退化为无隔离执行。

`vm-chroot` 的根：

1. 默认（持久）：构建含 `fuse` 且 `/dev/fuse` 存在 → FUSE 挂载 `vm_fs` 作为 sandbox 的根，写入当场落库。
2. 否则或 `--ephemeral` → `vm_materialize_tree` 到临时目录作为根；持久会话退出后 `vm_sync_from_host` 回写，`--ephemeral` 不回写并删除目录。

shell history 为 VM 内的 `/.ash_history`，随 VM 内容保存；`--ephemeral` 会话不保留。

sandbox 参数（四个命令通用）：

```sh
--isolation bwrap|gvisor      # 隔离后端，默认 bwrap；也可用 $SELF_SANDBOX_ISOLATION
--memory-mb N --pids N --cpu-percent N        # 整个 sandbox 的 cgroup 限制
--address-space-mb N --cpu-secs N --file-mb N # 每个进程的 rlimit
--no-limits                   # 不设任何限制，仅用于可信代码
```

默认值：内存 2048 MiB（`vm-agent` 为 4096）、进程数 1024、CPU 200%（两个核）、单文件 1024 MiB；`vm-agent` 另设地址空间 2048 MiB。

## 快照与维护

```sh
self vm-checkpoint /tmp/alpine.vm.db snap1 --note "after import"
self vm-snapshots  /tmp/alpine.vm.db
self vm-snapshot-file /tmp/alpine.vm.db s1     # 产物 <db>.snap.s1（VACUUM INTO）
self vm-restore-file  /tmp/alpine.vm.db s1
self vm-diff /tmp/alpine.vm.db snap1 snap2
self vm-gc /tmp/alpine.vm.db                    # WAL checkpoint(TRUNCATE) + VACUUM
self vm-recompress /tmp/alpine.vm.db            # 重新压缩旧 blob
self vm-compress-info /tmp/alpine.vm.db
self vm-cache-info; self vm-cache-prune --max 500M
```

## 同步与校验

```sh
self vm-sync /tmp/vm.db /path/to/tree          # 增量：只比较元数据（size、秒、ns、mode），未变文件不读内容
self vm-sync /tmp/vm.db /path/to/tree --full   # 全量：逐文件读内容并比对整文件 sha256，代价约为增量的 7 倍
self vm-verify /tmp/vm.db --deep               # 对账计数器、引用计数、分块完整性
```

- 增量同步依赖纳秒 mtime（schema v7）。升级前已存在且未再修改的行没有纳秒戳，只能靠秒级判断；`--full` 可以发现它们的陈旧内容。
- 文件在读取期间被修改时，该行不记录纳秒戳，下次同步会重读。

## 缓存上限

- 内容缓存与 bundle 缓存的上限默认为 2 GiB，由 `SELF_VM_CACHE_MAX` 设定（如 `512M`、`2G`，或纯字节数）。非法值会给出警告并使用默认值。
- 内容缓存在每次导入结束后淘汰（按最近使用时间，即 mtime）。
- bundle 缓存在每次启动后淘汰；正在运行的程序所用的条目持有共享锁，不会被淘汰。
- 淘汰只影响速度：缓存的每个文件都可以从数据库重建。

缓存目录 `cache_dir()`：`$SELF_VM_CACHE` → `$XDG_CACHE_HOME/self-vm` → `~/.cache/self-vm` → `$TMPDIR/self-vm-cache`。解压结果按 hash 存于 `<cache>/<aa>/<hash>`，物化时优先硬链。

## 内存镜像

```sh
self vm-mem-insert /tmp/alpine.vm.db 0x7fff0000 4096 5 /tmp/page.bin
self vm-mem-list   /tmp/alpine.vm.db
self vm-mem-trace  /tmp/alpine.vm.db /bin/echo -- hello   # strace 解析 mmap/mprotect 入 vm_mem（仅 Linux）
self vm-mem-clear  /tmp/alpine.vm.db
```

## 20G 稀疏块设备

```sh
self vm-init /tmp/vm.db --force --vm-only
self vm-disk-init   /tmp/vm.db --size 20G          # 稀疏：空洞不占存储
self vm-disk-import /tmp/vm.db /tmp/disk.raw       # 零块不落库
self vm-disk-info   /tmp/vm.db
self vm-disk-export /tmp/vm.db /tmp/out.raw        # 还原 raw 镜像
```

`vm-run` 调 QEMU 启动：

```sh
self vm-run /tmp/vm.db --mem 512M --raw /tmp/boot.raw            # 已有 raw 镜像
self vm-run /tmp/vm.db --kvm                                       # 无 --raw 时导出到 $TMPDIR
self vm-run /tmp/vm.db --kernel /boot/vmlinuz --initrd /boot/initrd.img --append "console=ttyS0 root=/dev/vda rw"
```

- QEMU 二进制：`$QEMU`，默认 `qemu-system-x86_64`。
- `--kernel` 直接内核启动；未指定则不加 `-kernel`，由磁盘 MBR/grub 引导。
- 只含 ext2 无 MBR 的盘会引导失败（`Boot failed: could not read the boot disk`），需 `--kernel` 或先装 bootloader。
- `--initrd` 留空即不使用 initrd；`--append` 缺省为 `console=ttyS0 root=/dev/vda rw panic=1`。

## 写入策略（vm-policy）

```sh
self vm-policy /tmp/vm.db add-readonly /tests        # 前缀及其下所有路径不可写
self vm-policy /tmp/vm.db add-max-size 1000000       # 常规文件结果大小上限（字节）
self vm-policy /tmp/vm.db list
self vm-policy /tmp/vm.db remove 1
```

- 规则存于 `vm_policies`，编译为 `vm_fs` 与 `vm_file_chunks` 上的 `BEFORE` 触发器。任何写入路径（包括直接 SQL）都会被数据库以 `EACCES: protected by vm_policies` 拒绝，语句整体回滚。
- 只读前缀按路径分量匹配：`/tests` 不覆盖 `/testsuite`。`/` 覆盖全部，但必须显式给出；空前缀会被拒绝。
- 级联删除同样被拒绝：删除受保护目录下的文件，连带的 chunk 删除也会失败。
- 快照恢复以绕过模式运行（每连接的 SQL 函数 `vm_policy_bypassed()`），保证回滚能修复受保护路径。绕过仅在调用它的连接上有效。
- 叠加层（Base/Delta）的写入在写前检查基础层的实时规则，因为规则存于 Base，而 Delta 的触发器看不到它。
- FUSE 把策略拒绝映射为 `EACCES`，其余写入错误为 `EIO`。

## 工具注入（vm-inject）

```sh
self vm-inject /tmp/vm.db /tmp/pkg.db     # 把包数据库（工具及其库闭包）合并进目标
```

- 单事务，合并后 `vm-verify --deep` 通过才提交，否则整体回滚。包数据库只读打开，不被修改。
- 不复制 `id` 列：行号是目标自身的，复制会与已有行冲突，`INSERT OR IGNORE` 会静默丢数据。
- 字典 id 按数据库独立分配：包用到的字典以新 id 追加到目标，blob 的 `dict_id` 相应重映射。
- 每个路径的 blob 在其 chunk 行挂接之前复制。预先复制会被其他路径的删除清理掉（refcount 为 0）。
- 同名路径被替换（旧行删除，其 chunk 引用经级联释放）。

## 单步执行（agent-step）

```sh
self vm-step /tmp/vm.db --timeout 60 --rollback-on-fail --max-output 1048576 -- /bin/sh -c 'make test'
self agent-log /tmp/vm.db [--json] [--last N]
```

每步：快照 `step-before` → 物化到临时目录 → 在 bwrap 中运行 → 同步回写 → 与快照比对得到变更清单 → 按需回滚 → 记录到 `agent_steps`（退出码、超时、输出截断标记、变更 JSON）。

- 没有 bwrap 时拒绝运行，不会退化为无隔离执行。
- 超时退出码 124；输出每个流最多保留 `--max-output` 字节，超出部分读取后丢弃，子进程不会因管道写满而阻塞。
- 变更清单经 JSON 转义，路径中的换行与引号可以正确还原。

## AgentVM：把 agent 装进 VM 跑

```sh
self vm-init work.db
self vm-provision-agent work.db --tools dev     # node + pi + git/coreutils/python3/make/gcc

self vm-pack  work.db /path/to/project --prefix /app
self vm-snap  work.db before

self vm-agent work.db --timeout 300 "修 /app 里失败的测试"        # 一次性任务，可审计可回滚
self agent-log work.db                                            # 审计
self vm-rollback work.db before                                    # 整体回滚
```

`--tools` 三档：`none`（只有 node 与 pi）、`minimal`（默认，加 coreutils）、`dev`（再加
git、sed/awk/find/tar/gzip、python3 及其标准库、make、gcc）。

- `dev` 会额外打包 gcc 的 cc1/as/ld/collect2、`/usr/include`、libgcc 与 C 库的 crt
  文件。这些不是 ELF 依赖，`vm-import` 的闭包看不到；缺了它们 gcc 只能打印版本号，
  任何真实编译都会失败。实测体积 248MB，VM 内可编译并运行 C 程序、`make` 可构建 C 目标。
- python3 需要标准库，其位置由宿主解释器自己报告（`sysconfig`），不是固定路径。

### 并行：每步一个 Delta

```sh
self vm-init work.db                       # 或复制一个已装好的库
self vm-agent work.db --base base.db -- "任务"    # work.db 作为只读 base.db 的 Delta
```

多个 Delta 可以同时跑，互不影响，Base 只读共享：每个 Delta 只存自己改过的 chunk，
实测一次只改一个文件的运行产生约 200KB 的 Delta（相对 115MB 的 Base）。

### 资源与配额

```sh
self vm-policy work.db add-readonly /tests
self vm-policy work.db add-max-size 1048576
self vm-policy work.db add-disk-quota 536870912    # 逻辑总字节上限
self vm-agent work.db --memory-mb 4096 --pids 512 --cpu-percent 200 --cpu-secs 120 --file-mb 256 "..."
```

- `--memory-mb` 是整个 sandbox 的 cgroup `memory.max`（禁用 swap），超出时内核 OOM 结束 sandbox 内的进程。
- `--address-space-mb` 是每个进程的 `RLIMIT_AS`，`vm-agent` 默认 2048：超出时 node 得到可捕获的 `RangeError`，
  而不是被 kill。node 需要 ≥1536 才能启动（低于此共享库映射失败）。
- 此前的 `--mem-mb`（即 `RLIMIT_AS`）已改名为 `--address-space-mb`。
- `disk_quota` 在**单库**和 **Delta** 两种形态下都生效：Delta 没有自己的策略表，
  叠加层在写前检查 Base 的规则（含只读前缀、单文件上限、总配额）。

### 受控包下载

VM 默认完全断网。需要下载时，宿主代理提供 `/pkg/<host>/<path>` 的白名单通道：

```sh
self vm-proxy --base "$MODEL_URL" --key-env MODEL_KEY /tmp/proxy.sock \
              --allow-hosts registry.npmjs.org,files.pythonhosted.org
self vm-agent work.db --base base.db --allow-hosts registry.npmjs.org -- "..."
```

- 只有白名单内的主机可达；其余返回 `403` 并在响应体说明原因。
- 白名单是宿主侧状态，guest 只能通过路径里的主机段表达目标，无法绕过。
- 包请求不携带模型凭据。
- guest 侧目前需要自己拼 `http://127.0.0.1:<port>/pkg/<host>/<path>`（VM 内没有
  `curl`，可用 python3 的 `http.client`）；没有 `HTTP_PROXY` 自动集成。

## 安全边界

本节说明每一层保证什么、不保证什么。这些是重要的，因为它们决定能把什么样的代码放进哪一层执行。

### 1. 沙箱（`src/sandbox.rs`，所有运行 VM 内容的命令）

隔离后端二选一：

| 后端 | 边界 | 状态 |
|---|---|---|
| bwrap（默认） | Linux 命名空间 + seccomp，与宿主共享内核 | 已测试 |
| gVisor（`--isolation gvisor`） | 用户态内核 `runsc`，sandbox 的 syscall 由它处理 | 已测试（runsc release-20261019.0，rootless） |

bwrap 后端的隔离：

- 命名空间：user、pid、net、ipc、uts、cgroup（`--unshare-all --unshare-user`），`--disable-userns` 禁止在 sandbox 内再建 user namespace；网络命名空间只有未启用的 `lo`。
- syscall 过滤：x86_64 seccomp 拒绝列表，包括模块加载、mount 系列、keyring、bpf、perf、userfaultfd、io_uring、namespace 创建（`unshare`、`setns`、带 `CLONE_NEW*` 的 `clone`）、时钟与系统状态修改；非 x86_64 的 syscall 表直接 kill。编号与内核头文件的一致性由测试检查。
- 环境变量：只有调用方显式给出的变量（`--clearenv`），宿主环境（含凭据）不进入 sandbox。
- 非交互运行使用 `--new-session`，sandbox 无法向调用方终端注入输入。
- `--die-with-parent`；超时或结束时整个 cgroup 内的进程都被结束，包括后台进程。

资源限制：

- 每个 sandbox 一个 cgroup v2：`memory.max`（禁用 swap）、`pids.max`、`cpu.max`。来源二选一：`$SELF_SANDBOX_CGROUP` 指向的委托 cgroup（服务以 `Delegate=yes` 运行时；本进程会移入其下的 `supervisor` 子组），或 systemd 用户会话的临时 scope。两者都没有时，设置了限制的 sandbox 拒绝启动。
- 每个进程的 rlimit：`RLIMIT_AS`、`RLIMIT_CPU`、`RLIMIT_FSIZE`。

不保证：

- bwrap 后端与宿主共享内核：seccomp 允许范围内的内核漏洞可越过边界。不可信的多租户场景应使用 gVisor 后端。

gVisor 后端：

- `runsc` 路径由 `$SELF_RUNSC` 指定，否则在 PATH 中查找。以 rootless 方式运行：`--network=none`、`--overlay2=none`（根目录的写入直接落到物化树，供回写）、`--host-uds=open`（agent 经绑定进来的 unix socket 访问宿主代理）。cgroup 限制与 bwrap 相同，施加在 runsc 外层；rlimit 写入 OCI 配置，由 gVisor 内核执行。
- OCI 配置中不声明 user namespace：`--rootless` 自行建立，在配置中声明会使 gofer 启动失败。
- 兼容性：git、python3（含 ssl/sqlite3）、gcc 编译运行、make、node、pi 均可运行（`tests/gvisor.rs`）。
- 实测耗时（物化树为根，dev 工具集，每项取 3 次最小值）：

  | 负载 | bwrap | gVisor | 倍数 |
  |---|---|---|---|
  | 启动 sandbox 并执行 `true` | 38 ms | 61 ms | 1.6 |
  | python3 启动 ×20 | 189 ms | 319 ms | 1.7 |
  | node 启动 ×20 | 1065 ms | 1698 ms | 1.6 |
  | gcc 编译 ×20 | 461 ms | 882 ms | 1.9 |
  | 复制 2k 文件的目录树 | 59 ms | 251 ms | 4.3 |
  | git add + commit 该树 | 190 ms | 581 ms | 3.0 |
  | tar 打包并解包 | 88 ms | 319 ms | 3.6 |
  | 遍历 2k 文件 ×20 | 88 ms | 310 ms | 3.5 |

  计算密集负载约慢 1.6–1.9 倍，文件密集负载约慢 3–4 倍。以 FUSE 挂载为根时的 gVisor 性能未测。
- 文件系统层面的 uid 隔离：sandbox 内仍以调用者身份访问其根目录。

### 2. 挂载（FUSE，`vm-mount`、`vm-chroot` 的 FUSE 路径）

- **所有者**：挂载视图中所有节点的属主都是挂载进程的 uid/gid，与进库来源无关。存储的 uid/gid 字段只记录来源，不参与访问控制。改属主的请求返回 `EPERM`。
- **符号链接**：挂载视图中，绝对链接目标改写为相对于链接所在目录的路径，由内核在挂载内解析。存储的链接目标不变。经挂载读取链接不会落到宿主文件（测试守护：链接指向宿主 `/etc/hostname` 时读取失败）。
- **权限位**：来自数据库（导入时的 mode），由内核按 `default_permissions` 强制执行。
- **不保证**：挂载**不是隔离边界**。它不限制进程能访问的宿主资源：
  - 符号链接改写只作用于 `readlink` 返回的文本。其他读取链接目标的接口未做改写，也未做测试。
  - 挂载内再挂载（嵌套挂载）未测试，其行为不作保证。
  - 进程仍可访问宿主的网络、进程表与其他文件。
- 结论：挂载本身不隔离任何东西。`vm-chroot` 以挂载为根时，进程运行在 §1 的 sandbox 内，隔离由 sandbox 提供；直接在宿主上访问挂载点的进程不受任何限制。

### 3. 数据库文件

VM 数据库、Base、vm-inject 的包都可能来自他人。所有连接都启用 SQLite 的防御设置：

- `SQLITE_DBCONFIG_DEFENSIVE`：禁止 `writable_schema` 与直接改写影子表。
- `trusted_schema=OFF`：文件中的触发器与视图只能调用标记为无副作用的函数。
- `cell_size_check=ON`：读取时检查页内 cell 边界。

`vm-inject` 以只读方式挂接包，合并前运行 `quick_check`，损坏的包被拒绝。

### 4. 策略与数据完整性

- 策略由数据库触发器强制，不依赖调用方的正确性。叠加层的检查是补充（见「写入策略」）。
- 注入与快照回滚都在事务内完成，失败时目标保持不变。
- 数据库本身可被任何能打开它的进程修改：文件权限是唯一的访问控制。VM 数据库应放在只有可信用户可读写的位置。

## 压缩与性能

- `compress_bytes` 按大小选 zstd 级别（≤16K:3，≤100K:6，其余:19），仅对 >1K 且有收益者压缩；小文件可走字典（`compressed=3`）。
- 实测 Alpine minirootfs：7.79M logical → 3.09M blob（lvl19）→ 3.2M db 文件。
- FUSE `statfs` 报告 `blocks = sum(blob)/4096`，因此 VM 内 `df` 显示压缩后大小。
- 经挂载的写入按 zstd level 3 压缩，不写宿主内容缓存（每次 write 都会重写所触及的 chunk）；`vm-recompress` 之后可把这些 blob 提到 level 19。
- FUSE 挂载实测（`/usr/include`，30,700 条目，312 MB；与原生文件系统对比）：

  | 操作 | 原生 | 挂载 |
  |---|---|---|
  | `find` 遍历 | 29 ms | 107 ms |
  | 读全部文件，首次 / 第二次 | 419 ms | 1.9 s / 1.0 s |
  | 写 2000 个小文件 | 28 ms | 401 ms |
  | 写一个 200 MB 文件 | 68 ms | 0.66 s |
  | 解包 30k 文件（tar） | 379 ms | 15 s |

  解包每个文件约 0.5 ms，其中数据库操作约 0.1 ms，其余是 FUSE 请求往返（守护进程单线程处理请求）。

## 实现位置

- `src/vm/`：存储层，按职责分为 `schema`/`codec`/`store`/`read`/`import`/`tree`/`snapshot`/`maint`/`cache`/`mem`/`disk`（模块说明见 `src/vm/mod.rs`）
- `src/fuse.rs`：`VmFuse`（fuser，`--features fuse`）
- `src/owner.rs`：挂载所有者、符号链接的挂载视图改写
- `src/policy.rs`：写入策略（触发器与叠加层检查）
- `src/inject.rs`：工具注入
- `src/agent.rs`：`agent-step` 单步执行、变更记录、回滚
- `src/overlay.rs`：Base/Delta 叠加层
- `src/bin/self.rs`：`vm-*` 子命令分发、`vm-chroot` / `vm-run` 逻辑

## 已知局限

- `vm-mem-trace` 依赖宿主 `strace`，仅 Linux。
- `vm-chroot` 回落路径会物化整个 rootfs 到临时目录，大镜像慢。
- FUSE 写入路径未做挂载端到端测试：本机无 root，挂载在用户命名空间内只能把挂载者映射为 uid 0，无法验证以非 0 uid 写入。见「安全边界」§2。
- gVisor 后端以 FUSE 挂载为根的组合未测试；`tests/gvisor.rs` 的兼容性与性能测试为 `#[ignore]`，需设置 `SELF_RUNSC` 后运行。
- `vm-agent --fuse` 是实验性的：能挂载、能启动 agent、/tmp 不留物化树。此前 pi 以 `O_EXCL` 打开会话文件失败，原因是 `create` 把 open(2) 的 flags 当作回复的 `FOPEN_*` flags 返回（`O_EXCL` 与 `FOPEN_PASSTHROUGH` 同为 bit 7），已修复；完整的 agent 运行尚未在挂载上验证，因此默认仍走物化路径。
- Delta 上跑 agent 时，快照 diff 只比较 Delta 自身，所以一个被修改的既有文件会显示为 `created` 而不是 `modified`（内容正确）。
- 包下载通道没有 guest 侧代理变量集成，VM 内也没有 `curl`；需自行发 `http://127.0.0.1:<port>/pkg/<host>/<path>`。
- 注入不检查目标是否正在使用；注入与一个正在运行的步骤同时进行时，行为未定义。
- 计划中的 `deny_exec_outside` 策略规则未实现。
- NBD 服务未实现（原 `vm-disk-nbd` 已删除）；需 `vm-disk-export` 后用 `qemu-nbd`。
