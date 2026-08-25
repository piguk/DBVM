# 单文件 VM

一个 `*.db` 即系统：`vm_fs` 存路径/权限/内容，`vm_mem` 存 memory image，`vm_meta/vm_log` 记元信息，`vm_snapshots` 存 checkpoint。所有操作为 SQL，文件/内存无二义。

- `vm_fs(path UNIQUE, kind, mode, uid, gid, mtime, size, link_target, content BLOB, hash)` + `idx_vm_fs_path` + `vm_mem/vm_snapshots/vm_log`
- `app_id VMSQ(0x564D5351) user_version 1`，`PRAGMA integrity_check` 即一致性校验

## 默认实例

`dbvm` 不带子命令即进入实例的 shell，实例不存在时自动 provision：

```sh
dbvm                    # 首次运行拉取 Alpine latest-stable 并导入，然后进 shell
dbvm run ls -la /etc    # 单条命令，退出码透传
dbvm status
```

实例路径按序取 `$DBVM_DB`、`$XDG_DATA_HOME/dbvm/default.db`、`~/.local/share/dbvm/default.db`；
`--db <path>` 可临时覆盖，`--arch` 指定非本机架构。下载的 tarball 缓存在 `$XDG_CACHE_HOME/dbvm`，
`reset --hard` 因此不必重新下载。

`run` 退出时把 scratch 目录的改动 sync 回同一个 `.db`，实例是有状态的：

```sh
dbvm run sh -c 'echo hi > /etc/marker'
dbvm cat /etc/marker     # hi
dbvm reset               # 回滚到 base snapshot，marker 消失
dbvm reset --hard        # 删库重建，重新拉 latest-stable
```

`base` 是 provision 时自动打的整库文件快照（`<db>.snap.base`）。`dbvm status` 会报告它是否存在。

## 执行模式

`dbvm run` 把整棵树 materialize 到 `/tmp/dbvm-XXXXXX`，按 bwrap → unshare → chroot 的顺序进入，
退出后 sync 回库并删除 scratch 目录。后端探测是实际试跑一次命名空间，而不是查命令是否存在——
容器里 `unshare` 常常存在却被内核拒绝。三者都不可用时报错，不做静默降级。

```sh
dbvm run                                    # /bin/sh
dbvm run sh -c 'uname -a; cat /etc/alpine-release'
dbvm run apk --version                      # apk-tools 3.0.6-r0
dbvm run busybox --list | wc -l             # 304
dbvm -v run true                            # 打印 scratch 目录、后端、sync 计数
```

`dbvm exec` 是无权限的替代路径：只 materialize 目标二进制与其依赖库，不进命名空间，
因此 guest 的 rootfs 与 PATH 不可见。CI 与 macOS 上用它。

```sh
dbvm exec /bin/busybox -- echo hi
dbvm exec /sbin/apk -- --version
# musl: /lib/ld-musl-<arch>.so.1（与 glibc 不同，LD_LIBRARY_PATH 仅对二次库生效，主解释器需显式派遣）
```

## 导入 Alpine

`scripts/fetch-alpine-rootfs.sh [dest-dir] [arch]` 从 `latest-stable` 解析当前 minirootfs、下载并校验 sha256，
输出 `ALPINE_BRANCH` / `ALPINE_VERSION` / `ALPINE_ARCH` / `ALPINE_TARBALL` 四个赋值行，可直接 `eval` 或追加到 `$GITHUB_ENV`。
arch 默认取本机 CPU（`uname -m` 映射到 Alpine 的命名，`arm64`/`aarch64` 均映射为 `aarch64`），
guest 二进制因此与 host 同架构，可直接执行；传第二个参数可拉取其他架构。
`dbvm init` 内部做同样的事（`src/fetch.rs`），两者保持一致。

```sh
# ~4M tar.gz -> ~8.7M db（musl 正确：/bin/sh -> /bin/busybox，ld-musl/libc.musl）
eval "$(scripts/fetch-alpine-rootfs.sh /tmp)"
dbvm --db /tmp/alpine.vm.db import-rootfs "$ALPINE_TARBALL"
dbvm --db /tmp/alpine.vm.db verify
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

## 其他操作

```sh
dbvm ls /etc                          # 目录列子项，文件列自身，缺失报错
dbvm cat /etc/alpine-release
dbvm stat /bin/busybox
dbvm materialize /tmp/alpine_root     # 落盘 83 files，含 /dev/proc/sys/tmp 兜底
bwrap --bind /tmp/alpine_root / --dev /dev --proc /proc --unshare-pid /bin/sh -c 'cat /etc/alpine-release'

# 快照与内存镜像（同库）
dbvm snapshot snap1 --note "after import"
dbvm snapshot snap1 --file            # 另存整库 -> <db>.snap.snap1
dbvm snapshots
dbvm restore snap1
dbvm mem insert 0x7fff0000 4096 5 /tmp/page.bin
dbvm mem list
sqlite3 ~/.local/share/dbvm/default.db "SELECT * FROM vm_mem LIMIT 5; PRAGMA integrity_check"
```
