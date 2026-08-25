# 代码布局

## 库

| 文件 | 内容 |
|---|---|
| `src/vm.rs` | `vm_fs/vm_mem/vm_snapshots/vm_meta/vm_log`；`vm_resolve` 40 跳 symlink 解析、`vm_materialize_tree` 三段落盘、`vm_sync_from_host`、`vm_snapshot_file`。`app_id VMSQ(0x564D5351)` |
| `src/instance.rs` | 默认实例路径、provision、`base` 快照、reset 与 reset --hard |
| `src/fetch.rs` | 解析 `latest-releases.yaml`、curl/wget 下载、sha256 校验、CPU → Alpine arch 映射 |
| `src/elf.rs` | ELF 解析（`goblin`），段/符号/节/notes/动态表 |
| `src/db.rs` | SELF 建库 |
| `src/closure.rs` | 闭包寻库：per-object 的 `RUNPATH/$ORIGIN` + `LD_LIBRARY_PATH` + dirname + 系统目录 |

## 可执行文件

| 文件 | 内容 |
|---|---|
| `src/bin/dbvm.rs` | 主入口。`run/exec/init/reset/status`、文件系统与快照命令、`self <sub>` 子树 |
| `src/bin/elf2self.rs` | ELF → SELF，含 `--bundle` 的 `bundle_objects/content` 与 `bundle_needs` |
| `src/bin/self-exec.rs` | `memfd` + `execve` 加载器；bundle 展开到 `/tmp/self-bundle-XXXXXX` 并前置 `LD_LIBRARY_PATH` |

## 其他

- `scripts/fetch-alpine-rootfs.sh` — 与 `src/fetch.rs` 等价的 shell 版本，CI 使用，改动需同步
- `examples/` — `queries.sql`、`strip.sql`、`patchelf.sql`、`preload.sql`、`self-ld.sql`、`closure.sh`、`greet/`
- `examples/bench/` — 体积与 userland 复现脚本

## 与原文的边界

覆盖：表结构与关键查询、`binfmt`、`closure` FK 去重、静态/动态加载、`scan/userland` 聚合与
`libself-audit` 桩。

未覆盖：纯 DB 内 `self-ld` 无落盘的完整实现；`R_X86_64_IRELATIVE` 直解交由 `ld.so` 完成
（历史上的 `mmap+auxv` 直跳实现见 git 历史）。
