# 运维任务包

内部系统的运维工作。模板只需 python3。

| family | 内容 | 判分 |
|---|---|---|
| `logs/incident` | 6 个互相调用的服务一小时的日志，注入一次故障：根因服务先报错，调用方随后连锁报错，另有带错误码的 WARN 干扰。写出根因服务、错误码与首次出错时间 | 按字段比较，部分得分 |
| `config/repair` | 一个读取 `/etc/app/app.ini` 的内部 HTTP 服务，配置中有 1–2 处错误（端口写错、数据目录错误、段名改动、端口不对）。修复配置 | 判分时用 agent 的配置启动服务，检查 `/health`、带 token 的 `/items`、错误 token 被拒，按通过比例 |
| `service/inventory` | 库存服务作为**任务服务**在 rollout 中运行（SQLite）。经 API 把低于补货线的商品补到目标量，其他不动 | 读取数据库，按商品比较库存 |

- 服务代码、日志在只读路径下；期望结果与判分脚本为判分专用文件。
- 库存服务在每个 rollout（含 fork 出的子 rollout）启动时启动并等待就绪；它启动时写入的内容不计入 agent 的改动。
- 作弊用例（得 0）：空 JSON 或非 JSON；空配置、让服务运行在别的端口；把所有库存清零、替换数据库文件。

```sh
packs/packkit/host-rootfs.sh python.tar python3      # 或 docker build packs/ops
self env-admin --data /srv/env image python python.tar
PYTHONPATH=packs:sdk/python python3 -m packkit build packs/ops http://env-node:8650 <template-id> --per-family 20
```
