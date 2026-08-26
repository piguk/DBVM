# SELF — Your executable is a SQLite database (Rust)

Rust 纯实现：把 ELF 转换为 `application_id = 0x53454C46 ("SELF")` 的 SQLite 数据库，提供查询与 `self-exec` 加载器，`objects/needs` 演示 closure 去重与可选表事务。

## 构建

```sh
cargo build --release
# 产物: target/release/elf2self target/release/self target/release/self-exec
```

## 快速开始

```sh
# 静态
gcc -static -no-pie -o /tmp/hello-static examples/hello_static.c
target/release/elf2self /tmp/hello-static -o /tmp/hello.self
target/release/self file     /tmp/hello.self
target/release/self ldd      /tmp/hello.self
target/release/self meta     /tmp/hello.self
sqlite3 /tmp/hello.self "SELECT sql FROM sqlite_master"

# 动态 + bundle（自包含，内嵌闭包 .so）
target/release/elf2self /bin/ls -o /tmp/ls.self
target/release/elf2self /bin/ls -o /tmp/ls.bundle.self --bundle
target/release/self bundle /tmp/ls.bundle.self
target/release/self bundle-info /tmp/ls.bundle.self
LD_LIBRARY_PATH="" target/release/self-exec /tmp/ls.bundle.self --version

# 查询（SQL 代替 readelf/nm/ldd）
target/release/self exports  /tmp/ls.self | head
target/release/self imports  /tmp/ls.self | head
target/release/self segments /tmp/hello.self
sqlite3 /tmp/hello.self < examples/queries.sql
sqlite3 /tmp/hello.self < examples/strip.sql   # DELETE + VACUUM，仍可运行

# 运行（统一 memfd+execve 委托宿主 ld.so，静态/动态同一路径）
target/release/self-exec /tmp/hello.self
target/release/self run /tmp/hello.self

# closure（/bin/ls -> 6 objects）
target/release/self closure /bin/ls /tmp/coreutils.db
sqlite3 -column /tmp/coreutils.db "SELECT n.soname, n.resolved_path FROM needs n JOIN objects o ON o.id=n.object_id WHERE o.is_root=1"

# 动态库示例（greet，复现 rm libgreet.so.1 三段式）
make -C examples/greet
bash examples/greet/demo.sh
bash examples/greet/interpose.sh

# 聚合 userland
target/release/self scan /tmp/scan.db /bin
target/release/self userland /tmp/userland.db /bin /usr/bin
bash examples/bench/userland.sh /tmp/userland.db /bin
bash examples/bench/size.sh
```

## 布局

- `src/elf.rs` / `src/db.rs` / `src/closure.rs` — ELF 解析（`goblin`）、SELF 建库、闭包寻库（`RUNPATH/$ORIGIN + LD_LIBRARY_PATH + dirname + 系统目录` per-object）
- `src/bin/elf2self.rs` — ELF → SELF（含 `--bundle` 的 `bundle_objects/content + bundle_needs`）
- `src/bin/self.rs` — `file/ldd/exports/imports/segments/meta/closure/scan/userland/bundle/bundle-info/pack/run`
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


## 单文件 VM（SQLite 即系统）—— Alpine 可跑

> 一个 `*.db` 既是文件系统也是内存镜像——`vm_blobs+vm_fs+vm_mem+vm_meta+vm_snapshots/vm_log` 同库（`VMSQ 0x564D5351`），`checkpoint` 即事务，`ATTACH/VACUUM/integrity_check` 即系统操作。Alpine musl 已跑通；`vm-chroot` 优先 **FUSE 直读**（`--features fuse` pure-rust，无 libfuse），无 `/dev/fuse` 回落物化+bwarp，内存 `strace trace=memory` 入 `vm_mem`。

```sh
# 1. 建库 & 导入 minirootfs（3.4M tar.gz -> 实测 3.6M db, 519 entries: 97 dirs 88 files 334 symlinks, logical 7.4M -> blob 3.4M 45.9%）
cargo build --release --features fuse
curl -L -o /tmp/alpine.tar.gz https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
target/release/self vm-init /tmp/alpine.vm.db --force
target/release/self vm-import-rootfs /tmp/alpine.vm.db /tmp/alpine.tar.gz --whitelist /bin --whitelist /lib # 裁剪至 ~2M
target/release/self vm-import-rootfs /tmp/alpine.vm.db /tmp/alpine.tar.gz
target/release/self vm-verify /tmp/alpine.vm.db  # integrity=ok page_count=920 files=519 bytes=7792927
sqlite3 /tmp/alpine.vm.db "SELECT sum(size)/1024/1024.0 ||'M logical' FROM vm_fs WHERE kind='file'"
./target/release/self vm-compress-info /tmp/alpine.vm.db
./target/release/self vm-status /tmp/alpine.vm.db

# 2. 进 VM（如何进入）
target/release/self vm-chroot /tmp/alpine.vm.db                    # 交互式；/dev/fuse 存在则 FUSE 零落盘（df 3.6M），否则物化 88 文件后 bwrap
target/release/self vm-chroot /tmp/alpine.vm.db /bin/sh -c 'cat /etc/alpine-release; ls /'
mkdir -p /tmp/mnt && target/release/self vm-mount /tmp/alpine.vm.db /tmp/mnt  # 显式 FUSE
df -h /tmp/mnt && cat /tmp/mnt/etc/alpine-release && fusermount -u /tmp/mnt

# 3. 单文件执行（按需 materialize 到 /tmp/self-vm-XXXXXX，自动处理 symlink 与 musl ld 派遣，DB 只读）
target/release/self vm-exec /tmp/alpine.vm.db /bin/sh -- -c 'echo hi; busybox echo hi2'
target/release/self vm-exec /tmp/alpine.vm.db /bin/busybox -- --help

# 4. 快照与内存镜像（同库：硬盘+内存均在同一 sqlite 不同表）
target/release/self vm-checkpoint /tmp/alpine.vm.db snap1 --note "after import"
target/release/self vm-snapshots /tmp/alpine.vm.db
target/release/self vm-mem-trace /tmp/alpine.vm.db /bin/echo -- hello  # strace 解析 mmap/mprotect 入 vm_mem
target/release/self vm-mem-list /tmp/alpine.vm.db
target/release/self vm-snapshot-file /tmp/alpine.vm.db snap_file1
sqlite3 /tmp/alpine.vm.db "SELECT id,addr,size,prot,length(content) FROM vm_mem; PRAGMA integrity_check"

# 5. 其他 vm-*（闭包/单文件粒度同样支持）
target/release/self vm-add /tmp/vm.db /bin/ls /bin/ls; target/release/self vm-import /tmp/vm.db /bin/ls
target/release/self vm-ls /tmp/vm.db; target/release/self vm-cat /tmp/vm.db /bin/ls > /tmp/out
```

> 完全独立验证：`vm-mount` 后 `df` 的 `Blocks/Used` 来自 FUSE `statfs = sum(blob)/4096 (~874 blocks = 3.6M)`，非宿主 `tmpfs 13.6G`；无 FUSE 时 `vm-chroot` 回落物化仍会自动 `vm_sync_from_host` 写回（WAL, 阈值 4M, `journal_size_limit 64M`）。压缩 `zstd:3` 仅对 `>1K +64` 有收益者生效，`LRU 64` + `SELF_VM_CACHE` 物化硬链加速。见 `docs/vm.md`。

体积：`alpine 7.4M logical -> 3.6M db (45.9%)`，`bundle ls 3.5M`；`vm-chroot(FUSE)` 零物化开销，回落分支 88 文件 rayon 并行。

实现：`src/vm.rs`（`vm_blobs` dedup/refcnt, `zstd`, `page_size 8192`, WAL, `vm_mem_trace` via `strace`）、`src/fuse.rs`（`VmFuse`, `statfs`, `staged->vm_add_bytes`, `pure-rust fuser`）、`src/bin/self.rs:Vm*`。

## 与原文边界

覆盖：表结构与关键查询、`binfmt`、`closure` FK 去重、静态/动态加载、`scan/userland` 聚合与 `libself-audit` 桩。
未覆盖：纯 DB 内 `self-ld` 无落盘完整实现、`R_X86_64_IRELATIVE` 直解由 `ld.so` 完成（历史 `mmap+auxv` 直跳见 git 历史）。

