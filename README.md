# SELF — 可执行文件即 SQLite 数据库

Rust 实现：把 ELF 转为 `application_id = 0x53454C46 ("SELF")` 的 SQLite 库，提供查询、闭包去重、自包含 bundle、`self-exec` 加载器，以及基于同一 SQLite 库的单文件 VM（`VMSQ`）。

## 构建

```sh
cargo build --release                  # self, elf2self, self-exec
cargo build --release --features fuse  # 额外启用 vm-mount / vm-chroot 的 FUSE 路径（纯 Rust fuser，无 libfuse）
```

产物：`target/release/{self,elf2self,self-exec}`。

## 快速开始

```sh
# 转换
gcc -static -no-pie -o /tmp/hello examples/hello_static.c
target/release/elf2self /tmp/hello -o /tmp/hello.self
target/release/self-exec /tmp/hello.self            # 运行
target/release/self run  /tmp/hello.self            # 同上（委托 self-exec）

# 查询（SQL 替代 readelf/nm/ldd）
target/release/self file     /tmp/hello.self
target/release/self ldd      /tmp/hello.self
target/release/self exports  /tmp/hello.self
target/release/self segments /tmp/hello.self
sqlite3 /tmp/hello.self < examples/queries.sql

# 精简（可选表 DELETE + VACUUM，仍可运行）
sqlite3 /tmp/hello.self < examples/strip.sql

# 动态库闭包
target/release/self closure /bin/ls /tmp/ls.db
sqlite3 -column /tmp/ls.db "SELECT n.soname, n.resolved_path FROM needs n JOIN objects o ON o.id=n.object_id WHERE o.is_root=1"

# 自包含 bundle（闭包 .so 内嵌，删除原库后仍可运行）
target/release/elf2self /bin/ls -o /tmp/ls.bundle.self --bundle
target/release/self bundle-info /tmp/ls.bundle.self
LD_LIBRARY_PATH="" target/release/self-exec /tmp/ls.bundle.self --version

# 多文件聚合去重
target/release/self scan /tmp/scan.db /bin
target/release/self userland /tmp/userland.db /bin /usr/bin
bash examples/bench/userland.sh /tmp/userland.db /bin
```

示例脚本：`examples/greet/demo.sh`（三段式演示）、`examples/greet/interpose.sh`（LD_PRELOAD）、`examples/closure.sh`、`examples/bench/size.sh`。

## 单文件 VM

完整说明与命令见 **[docs/vm.md](docs/vm.md)**。最小示例：

```sh
target/release/self vm-init /tmp/alpine.vm.db --force --vm-only
target/release/self vm-import-rootfs /tmp/alpine.vm.db /tmp/alpine.tar.gz
target/release/self vm-verify /tmp/alpine.vm.db
target/release/self vm-chroot /tmp/alpine.vm.db /bin/sh -c 'cat /etc/alpine-release'
target/release/self vm-exec /tmp/alpine.vm.db /bin/busybox -- --help
```

## RL 环境服务

以 HTTP 接口提供 RL 训练用的环境：模板、任务、批量 rollout、fork、checkpoint、判分、轨迹，多租户与配额，可多节点部署。见 **[docs/env.md](docs/env.md)**；部署见 [packaging/README.md](packaging/README.md)；EDA 任务包见 [packs/eda/README.md](packs/eda/README.md)。

```sh
self env-admin --data /srv/env image import base rootfs.tar
self env-serve --data /srv/env --listen 0.0.0.0:8650
```

## 布局

- `src/elf.rs` — ELF 解析（goblin）
- `src/db.rs` — SELF 建库（表与视图：`segments/symbols/needed/dynamic_entries/...`，视图 `ldd/exports/imports`）
- `src/closure.rs` — 依赖闭包寻库（RUNPATH/RPATH/LD_LIBRARY_PATH/调用方目录/系统目录）
- `src/vm/` — VM 存储层（`vm_fs`/`vm_blobs` 去重、zstd 字典、快照、`vm_mem`、4K 稀疏块设备），子模块划分见 `src/vm/mod.rs`
- `src/fuse.rs` — FUSE 挂载 `vm_fs`（`--features fuse`）
- `src/sandbox.rs` — 所有运行 VM 内容的 sandbox（bwrap / gVisor、seccomp、cgroup）
- `src/session.rs`、`src/session/` — 长期 session 与 sandbox 内的 supervisor（`init.c`，构建时静态链接，需要 C 编译器与 libc.a）
- `src/rollout.rs` — 以共享只读层为根的 overlay rollout
- `src/egress.rs` — build session 的出站白名单代理
- `src/env/` — RL 环境服务（控制库、存储、任务、HTTP 接口）
- `sdk/python/` — Python SDK
- `src/bin/self.rs` — 主 CLI（SELF 查询 + `vm-*` 子命令）
- `src/bin/self-exec.rs` — 加载器：`memfd` + `execve` 委托宿主 `ld.so`，bundle 展开
- `src/bin/elf2self.rs` — ELF → SELF 转换
- `docs/` — `closure.md`、`self-exec.md`、`binfmt.md`、`rtld-audit.md`、`vm.md`

## 限制

- 不实现纯 DB 内的 `self-ld` 链接器；动态重定位、TLS、IFUNC 仍由宿主 `ld.so` 完成。
- `bundle` 与 `self-exec` 依赖宿主 `ld.so`。
- `vm-mem-trace` 依赖 `strace`（仅 Linux）。
- `vm-run` 依赖宿主 QEMU（默认 `qemu-system-x86_64`，可用 `$QEMU` 覆盖）。
