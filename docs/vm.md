# 单文件 VM

一个 `*.db` 即系统：`vm_fs` 存路径/权限/内容，`vm_mem` 存 memory image，`vm_meta/vm_log` 记元信息，`vm_snapshots` 存 checkpoint。所有操作为 SQL，文件/内存无二义。

- `vm_fs(path UNIQUE, kind, mode, uid, gid, mtime, size, link_target, content BLOB, hash)` + `idx_vm_fs_path` + `vm_mem/vm_snapshots/vm_log`
- `app_id VMSQ(0x564D5351) user_version 1`，`PRAGMA integrity_check` 即一致性校验

## 导入 Alpine

`scripts/fetch-alpine-rootfs.sh [dest-dir] [arch]` 从 `latest-stable` 解析当前 minirootfs、下载并校验 sha256，
输出 `ALPINE_BRANCH` / `ALPINE_VERSION` / `ALPINE_ARCH` / `ALPINE_TARBALL` 四个赋值行，可直接 `eval` 或追加到 `$GITHUB_ENV`。
arch 默认取本机 CPU（`uname -m` 映射到 Alpine 的命名，`arm64`/`aarch64` 均映射为 `aarch64`），
guest 二进制因此与 host 同架构，可直接执行；传第二个参数可拉取其他架构。

```sh
cargo build --release
# ~4M tar.gz -> ~8.7M db（musl 正确：/bin/sh -> /bin/busybox，ld-musl/libc.musl）
eval "$(scripts/fetch-alpine-rootfs.sh /tmp)"
./target/release/self vm-init /tmp/alpine.vm.db --force
./target/release/self vm-import-rootfs /tmp/alpine.vm.db "$ALPINE_TARBALL"
./target/release/self vm-verify /tmp/alpine.vm.db
# integrity=ok page_size=4096 page_count=1044 freelist=0 files=515 bytes=8652792
```

<!-- alpine-verified:begin -->
CI 每周对 `latest-stable` 跑一次导入与执行，最近验证：Alpine 3.24.1。
<!-- alpine-verified:end -->

固定某个版本时直接写 URL：

```sh
curl -L -o /tmp/alpine.tar.gz \
  https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/alpine-minirootfs-3.24.1-x86_64.tar.gz
```

## 运行

```sh
# chroot 模式（推荐，bwrap 优先，其次 unshare --mount --map-root-user --root，最后 chroot）
# guest 参数一律放在 `--` 之后，否则 `-c` 会被当作 self 自己的 flag
./target/release/self vm-chroot /tmp/alpine.vm.db
./target/release/self vm-chroot /tmp/alpine.vm.db /bin/sh -- -c 'uname -a; cat /etc/alpine-release; ls /'
./target/release/self vm-chroot /tmp/alpine.vm.db /sbin/apk -- --version   # apk-tools 3.0.6-r0
./target/release/self vm-chroot /tmp/alpine.vm.db /bin/busybox -- --list | wc -l  # 304

# 单文件模式（按需 materialize 到 /tmp/self-vm-XXXXXX，自动处理 symlink 解析与 interpreter 派遣，DB 只读）
# 只落盘目标二进制与其依赖库；guest 的 rootfs 与 PATH 不可见，需要完整 rootfs 时用 vm-chroot
./target/release/self vm-exec /tmp/alpine.vm.db /bin/sh -- -c 'echo hi'
./target/release/self vm-exec /tmp/alpine.vm.db /bin/busybox -- --list | wc -l
# musl: /lib/ld-musl-x86_64.so.1（与 glibc 不同，LD_LIBRARY_PATH 仅对二次库生效，主解释器需显式派遣）

# 传统 vm-* 操作
./target/release/self vm-ls /tmp/alpine.vm.db /etc
./target/release/self vm-cat /tmp/alpine.vm.db /etc/alpine-release
./target/release/self vm-materialize /tmp/alpine.vm.db /tmp/alpine_root   # 落盘 83 files，含 /dev/proc/sys/tmp 兜底
bwrap --bind /tmp/alpine_root / --dev /dev --proc /proc --unshare-pid /bin/sh -c 'cat /etc/alpine-release'

# 快照与内存镜像（同库）
./target/release/self vm-checkpoint /tmp/alpine.vm.db snap1 --note "after import"
./target/release/self vm-snapshots /tmp/alpine.vm.db
./target/release/self vm-mem-insert /tmp/alpine.vm.db 0x7fff0000 4096 5 /tmp/page.bin
./target/release/self vm-mem-list /tmp/alpine.vm.db
./target/release/self vm-snapshot-file /tmp/alpine.vm.db snap_file1   # VACUUM INTO 或 cp，产物 /tmp/alpine.vm.db.snap.snap_file1
./target/release/self vm-restore-file /tmp/alpine.vm.db snap_file1
sqlite3 /tmp/alpine.vm.db "SELECT * FROM vm_mem LIMIT 5; PRAGMA integrity_check"
```
