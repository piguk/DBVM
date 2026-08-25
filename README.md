# DBVM

[![CI](https://github.com/piguk/DBVM/actions/workflows/ci.yml/badge.svg)](https://github.com/piguk/DBVM/actions/workflows/ci.yml)

一个 SQLite 文件装下完整的 Linux userland，可以直接进入其中的 shell。
文件系统、内存镜像和快照都是同一个库里的表。

```sh
cargo install --path .
dbvm
```

首次运行会拉取 Alpine minirootfs（校验 sha256）导入到 `~/.local/share/dbvm/default.db`，
然后进入 shell：

```
$ dbvm
-> no instance at ~/.local/share/dbvm/default.db
-> alpine 3.24.1 (aarch64) from latest-stable
-> imported 514 entries
/ # cat /etc/alpine-release
3.24.1
```

## 常用命令

```sh
dbvm                          # 交互 shell
dbvm run ls -la /etc          # 执行一条命令，退出码透传
dbvm status                   # 实例路径、体积、Alpine 版本
dbvm reset                    # 回到刚导入时的状态
```

命令参数直接写，不需要 `--`：`dbvm run sh -c 'echo hi'`、`dbvm run apk add curl` 都可以。

## 实例是有状态的

`dbvm run` 退出时会把改动写回同一个 `.db`，所以装的包、改的文件下次还在。

```sh
dbvm run apk add curl
dbvm run curl --version       # 仍然可用

dbvm reset                    # 回滚到导入后的状态，离线且很快
dbvm reset --hard             # 删库重建，重新拉取最新 Alpine
```

`reset` 依赖导入时自动打的 `base` 快照；`dbvm status` 会显示它是否存在。

## 从外面操作文件

不进 shell 也能读写实例里的文件：

```sh
dbvm ls /bin
dbvm cat /etc/alpine-release
dbvm cp ./myapp /usr/local/bin/myapp     # host -> 实例
dbvm extract /bin/busybox /tmp/busybox   # 实例 -> host
dbvm materialize /tmp/rootfs             # 整棵树落盘
```

## 快照

```sh
dbvm snapshot before-upgrade
dbvm snapshots
dbvm snapshot before-upgrade --file      # 另存整库 -> <db>.snap.before-upgrade
dbvm restore before-upgrade
```

## dbvm run 需要 bwrap、unshare 或 root

进入实例要用命名空间。容器、CI、macOS 上往往三者都不可用，这时用 `dbvm exec` 执行单个程序：

```sh
dbvm exec /bin/busybox -- echo hi
```

`exec` 只把目标二进制与其依赖库落盘，无需任何权限，代价是看不到实例里的其他文件。

## 多个实例

默认实例是 `~/.local/share/dbvm/default.db`（可用 `DBVM_DB` 改）。`--db` 指向任意其他文件，
不存在时自动创建：

```sh
dbvm --db ./project.db run sh          # 独立的一套环境
dbvm --db ./mini.db cp ./hello /hello  # 不带 rootfs，只放一个文件
dbvm --db ./x.db import-rootfs ./rootfs.tar.gz
```

`--arch` 可以拉取非本机架构（`x86_64`、`aarch64`、`armv7`…），`-v` 打印 scratch 目录、
使用的沙箱后端和 sync 计数。

## 文档

- [单文件 VM](docs/vm.md) — 表结构、执行模式、导入 Alpine 的细节
- [SELF](docs/self.md) — 把 ELF 装进数据库，`dbvm self` 与 `elf2self`
- [体积与性能](docs/benchmarks.md)
- [代码布局](docs/internals.md)
