# DBVM — 一个 SQLite 数据库就是整个系统

[![CI](https://github.com/piguk/DBVM/actions/workflows/ci.yml/badge.svg)](https://github.com/piguk/DBVM/actions/workflows/ci.yml)

一个 `*.db` 同时是文件系统、内存镜像和快照集：`vm_fs + vm_mem + vm_meta + vm_snapshots` 同库，
`checkpoint` 即事务，`ATTACH/VACUUM/integrity_check` 即系统操作。

```sh
cargo build --release
dbvm                    # 进入 shell；首次运行自动拉取 Alpine latest-stable
dbvm run ls -la /etc    # 在实例里执行一条命令
dbvm reset              # 回到导入后的状态
```

## 使用

```sh
dbvm                          # 交互 shell（无实例时自动 provision）
dbvm run <cmd> [args...]      # 在实例里执行，退出码透传，guest 参数无需 --
dbvm status                   # 实例路径、体积、Alpine 版本
dbvm reset                    # 回滚到 base snapshot（离线、快）
dbvm reset --hard             # 删库重建，重新拉取 latest-stable
```

实例默认位于 `~/.local/share/dbvm/default.db`，可用 `--db <path>` 或 `DBVM_DB` 覆盖，
`--arch` 指定非本机架构。`run` 退出时把改动 sync 回同一个 `.db`，所以实例是有状态的。

```sh
# 文件系统
dbvm ls /bin                  # 目录列子项，文件列自身
dbvm cat /etc/alpine-release
dbvm stat /bin/busybox
dbvm cp ./hello /hello        # host -> 实例（db 不存在时自动建 schema）
dbvm extract /bin/busybox /tmp/busybox
dbvm materialize /tmp/rootfs  # 整棵树落盘

# 快照
dbvm snapshot before-upgrade --note "3.24.1"
dbvm snapshot base --file     # 同时复制整库到 <db>.snap.base
dbvm snapshots
dbvm restore base

# 维护
dbvm verify                   # PRAGMA integrity_check
dbvm gc
dbvm compress                 # 压缩率与 page 布局
dbvm mem list
```

`dbvm run` 需要 bwrap、unshare 或 root。受限环境（容器、CI、macOS）里可以用
`dbvm exec <binary> -- [args]`：只落盘目标二进制与其依赖库，无需权限，但看不到 guest rootfs。

## SELF：把 ELF 装进数据库

`dbvm self <sub>` 保留原有的 ELF → SQLite 工具链，`application_id = 0x53454C46 ("SELF")`。

```sh
# 静态
gcc -static -no-pie -o /tmp/hello-static examples/hello_static.c
target/release/elf2self /tmp/hello-static -o /tmp/hello.self
dbvm self file /tmp/hello.self
dbvm self ldd  /tmp/hello.self
dbvm self meta /tmp/hello.self
sqlite3 /tmp/hello.self "SELECT sql FROM sqlite_master"

# 动态 + bundle（自包含，内嵌闭包 .so）
target/release/elf2self /bin/ls -o /tmp/ls.bundle.self --bundle
dbvm self bundle /tmp/ls.bundle.self
dbvm self bundle-info /tmp/ls.bundle.self
LD_LIBRARY_PATH="" target/release/self-exec /tmp/ls.bundle.self --version

# 查询（SQL 代替 readelf/nm/ldd）
dbvm self exports  /tmp/ls.self | head
dbvm self segments /tmp/hello.self
sqlite3 /tmp/hello.self < examples/queries.sql
sqlite3 /tmp/hello.self < examples/strip.sql   # DELETE + VACUUM，仍可运行

# 运行（统一 memfd+execve 委托宿主 ld.so，静态/动态同一路径）
dbvm self run /tmp/hello.self

# closure 与 userland 聚合
dbvm self closure /bin/ls /tmp/coreutils.db
dbvm self userland /tmp/userland.db /bin /usr/bin
bash examples/bench/size.sh
```

## 布局

- `src/vm.rs` — `vm_fs/vm_mem/vm_snapshots/vm_meta/vm_log`，symlink 解析、materialize、sync、快照
- `src/instance.rs` / `src/fetch.rs` — 默认实例与 provision；Alpine latest-stable 解析、下载、sha256 校验
- `src/elf.rs` / `src/db.rs` / `src/closure.rs` — ELF 解析（`goblin`）、SELF 建库、闭包寻库（`RUNPATH/$ORIGIN + LD_LIBRARY_PATH + dirname + 系统目录` per-object）
- `src/bin/dbvm.rs` — 主入口；`run/exec/init/reset/status`、文件系统与快照命令、`self <sub>` 子树
- `src/bin/elf2self.rs` — ELF → SELF（含 `--bundle` 的 `bundle_objects/content + bundle_needs`）
- `src/bin/self-exec.rs` — 统一 `memfd`+`execve` 加载器，`bundle` 时展开到 `/tmp/self-bundle-XXXXXX` 并前置 `LD_LIBRARY_PATH`
- `examples/` — `queries.sql/strip.sql/patchelf.sql/preload.sql/self-ld.sql/closure.sh/greet/`
- `examples/bench/size.sh` `examples/bench/userland.sh` — 单文件与 userland 复现
- `docs/binfmt.md` — `binfmt` 注册（内含 `self.conf`）
- `docs/` — `closure.md/rtld-audit.md/self-exec.md/binfmt.md`

## 性能

`hyperfine`（30 runs, warmup 10, `CARGO_TARGET_DIR=/tmp/cargo_target` Release `LTO/strip`）：

- `/tmp/hello-static` 原生: ~268–312 µs
- `self-exec /tmp/hello.rust.self` (Rust, `self_blob` 快路): ~2.1 ms（`mmap(MAP_SHARED)` 零拷 `copy_from_slice`，`openat 3`/`pread64 4`），体积 948K（原 741K，`segments.content=NULL` 仅 `self_blob` 存 758K，去重前 1.6M）
- `/bin/ls --version` 原生: ~749–768 µs
- `self-exec /tmp/ls.bundle.rust.self --version`: ~1.8–1.9 ms — 约 2.4×，bundle 仅私有库落盘（6 objects/9 edges, 3.5M）

`scan /bin`：`2490 ELFs -> 3089 objects 16134 needs 0.76s`（`FxHashMap` + `search_dirs_hash(u64)` 无 `join(":")` 分配，原 1.27–1.33s，`scan /bin` 约 40% 提升）。`Cargo.lock` 仅 `rustc-hash` 增量，`scan` 已加 `PRAGMA journal_mode=WAL / synchronous=NORMAL / BEGIN IMMEDIATE ... COMMIT` 事务批量。`strace -c` `hello` 总 111 syscalls。

`examples/bench/size.sh`：`ELF 158632 -> SELF 212992 (strip 后亦同)`，`BUNDLE 3579904 (bundle_objects=6)`。


## 自定义实例

`dbvm init` 拉的是 Alpine latest-stable。要自己控制 rootfs 来源：

```sh
# 从已有 tarball 导入（db 不存在时自动建 schema）
dbvm --db /tmp/alpine.vm.db import-rootfs /tmp/alpine-minirootfs.tar.gz
dbvm --db /tmp/alpine.vm.db verify        # integrity=ok files=515 bytes=8652792

# scripts/fetch-alpine-rootfs.sh 解析 latest-stable、下载并校验 sha256
eval "$(scripts/fetch-alpine-rootfs.sh /tmp)"   # 导出 ALPINE_VERSION / ALPINE_ARCH / ALPINE_TARBALL

# 只装一个二进制及其闭包，不要整个 rootfs
dbvm --db /tmp/mini.db import /bin/ls
dbvm --db /tmp/mini.db pack ./rootdir --prefix /

# 内存镜像与整库快照
dbvm mem insert 0x7fff0000 4096 5 /tmp/page.bin
dbvm snapshot snap1 --file   # VACUUM INTO 或 cp -> <db>.snap.snap1
dbvm restore snap1
sqlite3 ~/.local/share/dbvm/default.db "SELECT * FROM vm_mem LIMIT 5; PRAGMA integrity_check"
```

体积：`alpine 4.1M`（3.24.1 aarch64，content 压缩后的 db 文件大小），`ls 闭包`等同 `bundle 3.5M`，
`mini(3 files) 280K`；`exec` ~ musl interpreter 派遣开销，`run` ~ `bwrap` 绑定开销（`hyperfine` 15 runs warmup 5）。

实现：`src/vm.rs`（`VMSQ 0x564D5351`，`vm_fs/vm_mem/vm_snapshots/vm_meta/vm_log`，`vm_resolve` 40 跳 symlink 解析，
`vm_materialize_tree` 三段落盘，`vm_mem_*`/`vm_snapshot_file`）、`src/instance.rs`（默认实例、provision、reset）、
`src/fetch.rs`（latest-stable 解析、下载、sha256 校验）。

## 与原文边界

覆盖：表结构与关键查询、`binfmt`、`closure` FK 去重、静态/动态加载、`scan/userland` 聚合与 `libself-audit` 桩。
未覆盖：纯 DB 内 `self-ld` 无落盘完整实现、`R_X86_64_IRELATIVE` 直解由 `ld.so` 完成（历史 `mmap+auxv` 直跳见 git 历史）。

