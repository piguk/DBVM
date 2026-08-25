# 体积与性能

复现脚本：`examples/bench/size.sh`、`examples/bench/userland.sh`。

## 单文件 VM

| 项 | 数值 |
|---|---|
| Alpine 实例 | 4.1M（3.24.1 aarch64，content 压缩后的 db 文件） |
| `ls` 闭包 | 3.5M（等同 bundle） |
| mini（3 files） | 280K |

`dbvm exec` 的额外开销来自 musl interpreter 派遣，`dbvm run` 来自 `bwrap` 绑定与整棵树 materialize
（`hyperfine` 15 runs warmup 5）。

## SELF 加载

`hyperfine`（30 runs, warmup 10, Release `LTO/strip`）：

| 项 | 耗时 |
|---|---|
| `/tmp/hello-static` 原生 | ~268–312 µs |
| `self-exec /tmp/hello.self` | ~2.1 ms |
| `/bin/ls --version` 原生 | ~749–768 µs |
| `self-exec /tmp/ls.bundle.self --version` | ~1.8–1.9 ms（约 2.4×） |

`self_blob` 快路走 `mmap(MAP_SHARED)` 零拷 `copy_from_slice`，`openat 3` / `pread64 4`；
`hello` 总计 111 syscalls（`strace -c`）。体积 948K（`segments.content=NULL`，仅 `self_blob` 存 758K，
去重前 1.6M）。bundle 仅私有库落盘（6 objects / 9 edges, 3.5M）。

`examples/bench/size.sh`：`ELF 158632 -> SELF 212992`（strip 后亦同），`BUNDLE 3579904 (bundle_objects=6)`。

## 闭包扫描

`dbvm self scan /bin`：`2490 ELFs -> 3089 objects 16134 needs 0.76s`。

`FxHashMap` 加 `search_dirs_hash(u64)` 免去 `join(":")` 分配，从 1.27–1.33s 降下来约 40%；
写入侧用 `PRAGMA journal_mode=WAL / synchronous=NORMAL` 加 `BEGIN IMMEDIATE ... COMMIT` 批量事务。
