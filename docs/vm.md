# 单文件 VM

一个 `*.db` 即系统：`vm_fs` 存路径/权限/内容，`vm_mem` 存 memory image，`vm_meta/vm_log` 记元信息，`vm_snapshots` 存 checkpoint。所有操作为 SQL，文件/内存无二义。

- `vm_fs(path UNIQUE, kind, mode, uid, gid, mtime, size, link_target, content BLOB, hash)` + `idx_vm_fs_path` + `vm_mem/vm_snapshots/vm_log`
- `app_id VMSQ(0x564D5351) user_version 1`，`PRAGMA integrity_check` 即一致性校验

## 导入 Alpine

```sh
cargo build --release
# 3.4M tar.gz -> 7.7M db（517 entries: 97 dirs 87 files 334 symlinks，musl 正确：/bin/sh -> /bin/busybox，ld-musl/libc.musl）
curl -L -o /tmp/alpine.tar.gz https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
./target/release/self vm-init /tmp/alpine.vm.db --force
./target/release/self vm-import-rootfs /tmp/alpine.vm.db /tmp/alpine.tar.gz
./target/release/self vm-verify /tmp/alpine.vm.db
# integrity=ok page_size=4096 page_count=1946 freelist=0 files=518 bytes=7792915
```

## 运行

```sh
# chroot 模式（推荐，bwrap 优先，其次 unshare --mount --map-root-user --root，最后 chroot）
./target/release/self vm-chroot /tmp/alpine.vm.db
./target/release/self vm-chroot /tmp/alpine.vm.db /bin/sh -c 'uname -a; cat /etc/alpine-release; ls /'
./target/release/self vm-chroot /tmp/alpine.vm.db /sbin/apk -- --version   # apk-tools 2.14.4
./target/release/self vm-chroot /tmp/alpine.vm.db /bin/busybox -- --list | wc -l  # 304

# 单文件模式（按需 materialize 到 /tmp/self-vm-XXXXXX，自动处理 symlink 解析与 interpreter 派遣，DB 只读）
./target/release/self vm-exec /tmp/alpine.vm.db /bin/sh -- -c 'echo hi; busybox echo hi2'
./target/release/self vm-exec /tmp/alpine.vm.db /bin/busybox -- --help
# musl: /lib/ld-musl-x86_64.so.1（与 glibc 不同，LD_LIBRARY_PATH 仅对二次库生效，主解释器需显式派遣）

# 传统 vm-* 操作
./target/release/self vm-ls /tmp/alpine.vm.db /etc
./target/release/self vm-cat /tmp/alpine.vm.db /etc/alpine-release
./target/release/self vm-materialize /tmp/alpine.vm.db /tmp/alpine_root   # 落盘 87 files，含 /dev/proc/sys/tmp 兜底
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
