# 单文件 VM

一个 `*.db` 即系统：`vm_fs` 存路径/权限/内容，`vm_mem` 存 memory image，`vm_meta/vm_log` 记元信息，`vm_snapshots` 存 checkpoint。所有操作为 SQL，文件/内存无二义。

- `vm_fs(path UNIQUE, kind, mode, uid, gid, mtime, size, link_target, hash, compressed, content BLOB)` 经 `vm_blobs(hash PK, content BLOB, compressed, raw_size, refcnt)` 去重 (`content` 置 `NULL`，`compressed 0=raw 1=gz 2=zstd 3=zstd+dict`) + `idx_vm_fs_path/hash`, `idx_vm_blobs_refcnt`
- 新库 `page_size 8192 auto_vacuum=INCREMENTAL` (`init_vm_db` 时置)，`--vm-only` 跳过 `self_* / segments / symbols / bundle_*` 表以省 70K；既有库保持 4096 直至 `VACUUM`；`vm_apply_pragmas: journal_mode=WAL synchronous=NORMAL temp_store=MEMORY cache_size=-64000 mmap_size=256M busy_timeout=5000 journal_size_limit=64M`
- `vm_dict(id=1, dict BLOB, samples, dict_size)` 存放 zstd 字典，`app_id VMSQ(0x564D5351) user_version 1`，`PRAGMA integrity_check` 即一致性校验

## 导入 Alpine

```sh
cargo build --release --features fuse   # pure-rust fuser default-features=false, 无需 libfuse3-dev
curl -L -o /tmp/alpine.tar.gz https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
./target/release/self vm-init /tmp/alpine.vm.db --force --vm-only   # --vm-only 跳过 SELF/bundle 表，省 ~70K，纯 VM
./target/release/self vm-import-rootfs /tmp/alpine.vm.db /tmp/alpine.tar.gz --whitelist /bin --whitelist /etc --whitelist /lib --exclude /usr/share/apk/keys
# 或者全量导入后自动训练字典（小文件受益）：
./target/release/self vm-import-rootfs /tmp/alpine.vm.db /tmp/alpine.tar.gz --exclude /usr/share/doc --exclude /var/cache
./target/release/self vm-train-dict /tmp/alpine.vm.db --max-size 16384   # 采集 10..120 个 256..16K 样本，训练 16K dict 存入 vm_dict
./target/release/self vm-dict-info /tmp/alpine.vm.db
./target/release/self vm-verify /tmp/alpine.vm.db
# 新库实测：7.79M logical -> 3.09M blob (39.7%, lvl 19) -> 3.2M db file (408*8192)，比旧 3.6M(4096*920) 小 0.5M
# --whitelist 裁剪或 /bin/busybox 单文件可至 168K..600K（busybox 80K 极简定制 vs 790K Alpine musl）
sqlite3 /tmp/alpine.vm.db "SELECT sum(size)/1024/1024.0 ||'M logical' FROM vm_fs WHERE kind='file'"
./target/release/self vm-compress-info /tmp/alpine.vm.db
./target/release/self vm-status /tmp/alpine.vm.db
# 300K 达成条件：`/bin/busybox` 单文件需定制 tinyconfig（79K raw -> 42K zstd -> 168K db with --vm-only），Alpine 官方 790K 需 UPX(~420K) 且仍约 560K db
```

## 运行：如何进 VM

```sh
# 交互式进入（优先 FUSE，无 /dev/fuse 则回落 materialize；默认 --persist，--ephemeral 显式关闭）
./target/release/self vm-chroot /tmp/alpine.vm.db                      # 默认 persist: history 自动落 cache/history/<db_hash>/.ash_history
./target/release/self vm-chroot /tmp/alpine.vm.db /bin/sh -c 'uname -a; cat /etc/alpine-release; ls /'
./target/release/self vm-chroot /tmp/alpine.vm.db --persist /bin/sh   # 显式 --persist: HISTFILE=.ash_history + PS1=vm:\w\$ + 保留 tmp 目录
./target/release/self vm-chroot /tmp/alpine.vm.db --ephemeral /bin/sh -c 'echo tmp-only; cat /etc/alpine-release'  # 不写回 vm_fs, 删 tmp，不持久 history
./target/release/self vm-chroot /tmp/alpine.vm.db /sbin/apk -- --version
./target/release/self vm-chroot /tmp/alpine.vm.db /bin/busybox -- --list | wc -l

# FUSE 显式挂载（需宿主 /dev/fuse）
mkdir -p /tmp/mnt && ./target/release/self vm-mount /tmp/alpine.vm.db /tmp/mnt
df -h /tmp/mnt        # statfs blocks=sum(blob)/4096 -> 3.6M, 非宿主 tmpfs 13.6G
ls /tmp/mnt/etc/alpine-release
fusermount -u /tmp/mnt   # 或 umount

# 单文件模式（按需 materialize 到 /tmp/self-vm-XXXXXX，自动处理 symlink 与 musl interpreter 派遣，DB 只读）
./target/release/self vm-exec /tmp/alpine.vm.db /bin/sh -- -c 'echo hi; busybox echo hi2'
./target/release/self vm-exec /tmp/alpine.vm.db /bin/busybox -- --help

# 传统 vm-* 操作
./target/release/self vm-ls /tmp/alpine.vm.db /etc
./target/release/self vm-resolve /tmp/alpine.vm.db /bin/sh   # 调试 40 跳 vm_resolve -> /bin/busybox 实路径
./target/release/self vm-cat /tmp/alpine.vm.db /etc/alpine-release
./target/release/self vm-materialize /tmp/alpine.vm.db /tmp/alpine_root
bwrap --bind /tmp/alpine_root / --dev /dev --proc /proc --unshare-pid /bin/sh -c 'cat /etc/alpine-release'
```

### 独立性说明（FUSE vs materialize）

- **FUSE 直读**：`open/read/write -> SELECT content FROM vm_blobs WHERE hash` 按需 `zstd` 解压，经 LRU(64) 与 `SELF_VM_CACHE=/tmp/self-vm-cache/<aa>/<hash>` 硬链加速；`statfs` 返回 `f_blocks = sum(blob)/4096`，故 VM 内 `df` 显示 3.6M 真相而非宿主 13.6G。`vm-chroot` 探测到 `/dev/fuse` 即 `spawn_mount2` 后 `bwrap --bind <fuse_mnt> /`，全程不走 `/tmp` 落盘。
- **回落**：沙箱/CI 无 `/dev/fuse` 时 `vm-chroot` 回落 `vm_materialize_tree` (rayon 并行 + size 快速跳过 + cache 硬链) 再 `bwrap --bind /tmp/self-vm-* /`；`df` 此时仍显示宿主 tmpfs。更新自动落盘：`vm-chroot` 退出后 `vm_sync_from_host` 比对 `hash/size/mode/link_target`，新/改文件经 `vm_add_bytes` (dedup+refcnt) 寫回，WAL>4M 触发 `wal_checkpoint(PASSIVE)`，常用 `PRAGMA journal_size_limit 64M`。

## 快照与内存镜像（同库）

```sh
./target/release/self vm-checkpoint /tmp/alpine.vm.db snap1 --note "after import"
./target/release/self vm-snapshots /tmp/alpine.vm.db
./target/release/self vm-mem-insert /tmp/alpine.vm.db 0x7fff0000 4096 5 /tmp/page.bin
./target/release/self vm-mem-list /tmp/alpine.vm.db
./target/release/self vm-mem-trace /tmp/alpine.vm.db /bin/echo -- hello   # strace -f -tt -e trace=memory 解析 mmap/mprotect 等入 vm_mem
./target/release/self vm-snapshot-file /tmp/alpine.vm.db snap_file1   # VACUUM INTO 或 cp，产物 /tmp/alpine.vm.db.snap.snap_file1
./target/release/self vm-restore-file /tmp/alpine.vm.db snap_file1  # 自动 VACUUM + incremental
./target/release/self vm-gc /tmp/alpine.vm.db   # 若 page_size 仍 4096 则迁移至 8192 + VACUUM
./target/release/self vm-diff /tmp/alpine.vm.db snap1 bench   # 快照 diff + 最近 vm_log
./target/release/self vm-cache-info; ./target/release/self vm-cache-prune --max 500M
./target/release/self vm-status /tmp/alpine.vm.db  # WAL/mmap/压缩比/cache 一览
sqlite3 /tmp/alpine.vm.db "SELECT id,addr,size,prot,length(content) FROM vm_mem LIMIT 5; PRAGMA integrity_check"
```

## 压缩 / WAL / 缓存物化

- **压缩**：`compress_bytes` 按大小自适应 `lvl 3(≤16K)/6(16..100K)/19(>100K)` 且 `+64` 阈值，仅对大于 1K 者压为 `compressed=2`；`compress_bytes_with_conn` 额外尝试 `compressed=3(dict)`（`vm_dict` 训练后对 `<16K` 小文件有效，字典来自 `vm_train_dict`）；`vm_recompress` 迁移旧 `gz`/`inline` 并对 `plain`/`zstd lvl3` 尝试升至 `19`；`vm_compress_info` 展示 `logical / blob_storage / ratio / db file`。
- **WAL**：`vm_apply_pragmas` 设 WAL+NORMAL+64M limit，`vm_sync_from_host` 阈值 4M 节流 `wal_checkpoint(PASSIVE)`，`vm_gc` 执行 `wal_checkpoint(TRUNCATE)+VACUUM`。
- **缓存**：`decompress_cache LRU 64` + `cache_dir` (`SELF_VM_CACHE > XDG_CACHE_HOME > HOME/.cache/self-vm > XDG_RUNTIME_DIR > /tmp/self-vm-cache`) 存放解压后明文，`vm-materialize` 优先 `hardlink -> copy -> write`，`vm-cache-{info,prune}` 管理。
- **验证**：`vm-status` 汇总 `integrity/page_size/page_count/freelist/journal/mmap` 与 `files/logical/blob_storage/blobs/compressed/cached`；`df` 语义由 FUSE `statfs` 保证。

## 性能体积对照

- Alpine 当前实测（lvl19 + vm_only）：`7.79M logical -> 3.09M blob (39.7%) -> 3.2M db file (408*8192)`；旧库 `7.43M -> 3.41M (45.9%) -> 3.6M (920*4096)` 对照。`--whitelist` 裁剪后可至 ~2.9M 内；极致 `/bin/busybox` 官方 790K -> 458K zstd19 -> 568K db（含 70K 元信息）/`--vm-only` 518K，tinyconfig 79K -> 42K -> 168K db（<300K 达成）；`vm_materialize_tree` 二次物化 mtime+size 快路径 5ms 级（首轮 rayon 并行）；`vm-restore-file`/`vm-gc` 自动 VACUUM。
- `vm-exec` ~ musl interpreter 派遣开销，`vm-chroot(FUSE)` 零物化开销（仅首读解压），回落分支为 88 文件并行物化。

实现：`src/vm.rs` (`VMSQ`, `vm_blobs/vm_fs/vm_mem/vm_dict`, `vm_resolve` 40 跳, `vm_materialize_tree`, `vm_sync_from_host`, `compress_bytes_with_conn`)、`src/fuse.rs` (`VmFuse`, `statfs`, `staged flush -> vm_add_bytes`)、`src/bin/self.rs:Vm*`。

## 20G 分页块设备（完整 VM/内核路线 B）

单一 `*.db` 调度硬盘按 4K 分页稀疏 `vm_disk_blocks(block_id PK, content BLOB, compressed, raw_size)`，空洞零开销；`vm_meta.disk_size` 记录逻辑大小（需为 4K 倍数），`disk_block_size=4096`。

```sh
# 1. 预分配 20G（稀疏，空库 136K）
./target/release/self vm-init /tmp/vm.db --force --vm-only
./target/release/self vm-disk-init /tmp/vm.db --size 20G
./target/release/self vm-disk-info /tmp/vm.db            # blocks=5242880 stored=0 sparse_hole=20G
./target/release/self vm-status /tmp/vm.db                # 同时显示 files/blob + disk

# 2. 从 raw 镜像导入（稀疏写：零页不落库，zstd 页按需压）
/sbin/mke2fs -d /tmp/mini_root -t ext2 -b 1024 -m 0 -O ^has_journal -F /tmp/disk.raw 32M   # 宿主造盘
./target/release/self vm-disk-import /tmp/vm.db /tmp/disk.raw                         # 已存在 20G 时不收缩；指定 --size 可显式扩/缩
sqlite3 /tmp/vm.db "SELECT count(*) FROM vm_disk_blocks"  # 32M 示例仅 148 块存储

# 3. 以块为粒度读写（用于增量 sync/NBD）
python3 - <<'PY2'
import rusqlite
# 等价 SQL：vm_disk_read/write 暴露为 API，CLI 可扩展 vm-disk-read/write
PY2

# 4. 导出 raw（稀疏 seek：20G 空洞零成本，仅回放 148 块；落盘后可用 loop/NBD 接内核）
./target/release/self vm-disk-export /tmp/vm.db /tmp/vm.db.raw   # set_len(disk) + pwrite 已压缩块
cmp /tmp/disk.raw /tmp/vm.db.raw && echo ok
ls -lh /tmp/vm.db /tmp/vm.db.raw   # db 约 952K vs raw 32M 稀疏感知仅 400K 压缩存储

# 5. 跑完整 kernel（需宿主 qemu-system-x86_64）
apt install qemu-system-x86  # 宿主（提供 qemu-system-x86_64 / qemu-system-i386 二者皆可，本项目自动探测）
qemu-img create -f raw /tmp/empty.raw 20G
./target/release/self vm-disk-export /tmp/vm.db /tmp/boot.raw   # 或直接用空洞 raw 接 NBD
qemu-system-x86_64 -m 512M -drive file=/tmp/boot.raw,format=raw,if=virtio -serial mon:stdio -nographic  # 若宿主仅有 qemu-system-x86，二进制为 /usr/bin/qemu-system-i386，vm-run 会自动探测并可用 QEMU=/usr/bin/qemu-system-i386 覆盖
# NBD 稀疏按需（避免导出 20G 实体）：
qemu-nbd --shared=4 -x selfdisk -f raw /tmp/boot.raw -p 10809 -t &
qemu-system-x86_64 -m 512M -drive file=nbd:localhost:10809/1,format=raw,if=virtio -serial mon:stdio -nographic
# vm-run 便捷壳（自动 export -> qemu）：
./target/release/self vm-run /tmp/vm.db --mem 512M --raw /tmp/boot.raw   # 或不带 --raw 则临时 /tmp/self-vm-disk-*.raw
./target/release/self vm-run /tmp/vm.db --mem 1G --kvm                       # 宿主有 /dev/kvm 时加 --kvm
QEMU=/usr/bin/qemu-system-i386 ./target/release/self vm-run /tmp/vm.db          # 覆盖探测
QEMU_SYSTEM_X86_64=/usr/bin/qemu-system-x86_64 ./target/release/self vm-run /tmp/vm.db  # 显式指定
```

内存亦在同表：`vm_mem(addr,size,prot,content)` 记录 `mmap/mprotect` image，`vm-mem-trace` 自 `strace` 解析；`vm_snapshots/vm_log` 则作 checkpoint。`vm_fs + vm_mem + vm_disk_blocks` 同库即“硬盘+内存全在同一 sqlite 不同表/分页”，WAL+`journal_size_limit 64M` 保证事务安全，`vm_gc` 回收稀疏空洞。

局限：QEMU 侧需宿主真实 qemu / kvm；NBD 真正零拷贝需 nbd-server 将 `vm_disk_blocks` 作为后端（当前为 `vm-disk-export + qemu-nbd` 二段，待补 `rusteNBD` 直读）。

