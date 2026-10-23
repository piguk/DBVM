# 单文件 VM

一个 `*.db` 即系统。SQLite 表分工：

- `vm_fs(path UNIQUE, kind, mode, uid, gid, mtime, size, link_target, hash, compressed, content)`：路径/权限/内容
- `vm_blobs(hash PK, content, compressed, raw_size, refcnt)`：内容去重，`compressed 0=raw 2=zstd 3=zstd+dict`
- `vm_dicts`：zstd 字典，追加写入、不覆盖（`vm-train-dict` 每次新增一行）。压缩块的 `vm_blobs.dict_id` 记录其所用字典，因此重训后旧块仍可读
- `vm_meta`：O(1) 计数器（文件数、逻辑字节、blob 字节），由触发器维护，`vm-verify --deep` 可逐项对账
- `vm_mem`：内存镜像；`vm_snapshots` / `vm_log`：checkpoint 与变更日志
- `vm_disk_blocks(block_id PK, content, compressed, raw_size)`：4K 稀疏分页块设备；`vm_meta.disk_size` 记录逻辑大小
- 库 `application_id = 0x564D5351 ("VMSQ")`，`user_version = 6`（当前 schema）。旧库在打开时按序迁移，每步一个事务；历史版本见 `src/vm.rs` 顶部注释

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
self vm-chroot /tmp/alpine.vm.db --ephemeral /bin/sh -c 'echo tmp-only'   # 不回写、不保存 history
self vm-exec   /tmp/alpine.vm.db /bin/busybox -- --help                  # 单文件：仅 materialize 所需文件
self vm-mount  /tmp/alpine.vm.db /tmp/mnt                                 # FUSE 挂载（需 /dev/fuse，--features fuse）
```

`vm-chroot` 选择顺序：

1. 构建含 `fuse` 且 `/dev/fuse` 存在 → FUSE 挂载 `vm_fs`，再 `bwrap --bind <mnt> /`，不落盘到 `/tmp`。
2. 否则 → `vm_materialize_tree` 到 `$TMPDIR/self-vm-XXXXXX`，`bwrap` 或 `unshare` 进入；退出后 `vm_sync_from_host` 把改动写回 DB。

`bwrap` 缺失 → `unshare`；再缺失 → `chroot`（需 root）。

`--persist`（默认）：history 存 `<cache>/history/<db_hash>/.ash_history`。`--ephemeral`：不回写、不保存 history，删除临时目录。

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

## 压缩与性能

- `compress_bytes` 按大小选 zstd 级别（≤16K:3，≤100K:6，其余:19），仅对 >1K 且有收益者压缩；小文件可走字典（`compressed=3`）。
- 实测 Alpine minirootfs：7.79M logical → 3.09M blob（lvl19）→ 3.2M db 文件。
- FUSE `statfs` 报告 `blocks = sum(blob)/4096`，因此 VM 内 `df` 显示压缩后大小。

## 实现位置

- `src/vm.rs`：存储层、导入导出、压缩、快照、磁盘块
- `src/fuse.rs`：`VmFuse`（fuser，`--features fuse`）
- `src/bin/self.rs`：`vm-*` 子命令分发、`vm-chroot` / `vm-run` 逻辑

## 已知局限

- `vm-mem-trace` 依赖宿主 `strace`，仅 Linux。
- `vm-chroot` 回落路径会物化整个 rootfs 到临时目录，大镜像慢。
- FUSE 写入经 `staged` 缓冲，`flush` 时写入 DB。
- NBD 服务未实现（原 `vm-disk-nbd` 已删除）；需 `vm-disk-export` 后用 `qemu-nbd`。
