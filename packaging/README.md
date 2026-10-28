# 部署

## 单机或 systemd 集群（tarball）

```sh
packaging/build-tarball.sh                                    # 本机 glibc 版本；产物在 target/dist/
TARGET=x86_64-unknown-linux-musl packaging/build-tarball.sh  # 静态版本，任意发行版可用（需要 musl target）
```

tarball 内含 `bin/self`、systemd unit、`install.sh`、`docs/env.md` 与 `REQUIREMENTS`（glibc 版本要求等）。在目标机器上（root）：

```sh
tar xzf selfdb-*.tar.gz && cd selfdb-*/
./install.sh node            # 或 ./install.sh coordinator
vi /etc/selfdb/node.env      # SELFDB_NODE_ARGS：--node、--listen、--max-sessions；集群中还有 --coordinator、--advertise
systemctl enable --now selfdb-node
```

- `install.sh` 创建系统用户 `selfdb` 与 `/var/lib/selfdb`（0700），安装 unit；若没有 `/etc/selfdb/cluster-token`，生成一个随机 token（root，0600），由 systemd 以 credential 交给服务。集群中所有机器使用同一个 token 文件。
- 节点服务 `Delegate=yes` 并设置 `SELF_SANDBOX_CGROUP=auto`：每个 sandbox 一个子 cgroup；`OOMPolicy=continue`：sandbox 内存耗尽只结束该 sandbox。
- `self env-doctor --data DIR` 检查内核、cgroup v2、user namespace（含 Ubuntu 的 AppArmor 限制）、bwrap 版本、overlay sandbox、inotify 上限与数据目录的文件系统；`install.sh` 安装节点时会运行一次。
- 指标：`--metrics-listen ADDR` 在 `http://ADDR/metrics` 提供 Prometheus 指标，不鉴权，应只监听内网地址。
- 诊断包：`self env-admin --data DIR support-bundle out.tar.gz`，含检查结果、版本、数据目录大小与控制库摘要（计数、近期 session 的 id/时间/结束原因），不含 API key、token、租户和模板名称、label、任务内容。

## Kubernetes

`packaging/Dockerfile` 构建静态二进制的 Alpine 镜像；`packaging/k8s/selfdb.yaml` 为一个 coordinator（StatefulSet）与 2 个节点（StatefulSet，节点 id 为 `n<序号>`）。节点容器为 `privileged`：sandbox 需要 user namespace（seccomp 与 AppArmor 不加限制）、自己的 `/proc` 挂载和可写的 cgroup 树。容器内的 root 即宿主机 root，节点应放在专用机器上。

## 未验证的部分

| 部分 | 状态 |
|---|---|
| tarball 构建、systemd unit 语法（`systemd-analyze verify`）、`install.sh` 语法 | 已在构建机上检查 |
| 以 systemd 服务运行（`Delegate=yes`、credential、`ProtectSystem=strict`） | 未在真实服务中运行：构建机上没有 root。`SELF_SANDBOX_CGROUP=auto` 已在委托的 systemd scope 中验证（`tests/env_perf.rs`） |
| musl 静态构建、Docker 镜像 | 未构建：构建机没有网络，也没有 musl target |
| Kubernetes manifest | YAML 可解析；未在集群上运行 |
