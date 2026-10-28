# packkit

领域任务包的生成、校验与检查。一个包是一个目录，其中的 `pack.py` 定义 `candidates(rng)`：按 family（如 `rtl/alu`、`sql/top_customers`）返回候选任务的迭代器。每个候选（`packkit.Candidate`）包含：

- `spec`：任务定义（不含 `template_id`）：指令、任务文件、判分命令与判分专用文件、服务、只读路径等；
- `solution`：解答文件，用于校验与检查；
- `cheats`：作弊用例，每个是一组文件，判分必须为 0；
- `meta`：写入 `tasks.json` 的附加信息。

可选的 `start_ok(verdict, candidate)` 决定起始状态是否合格（默认：reward < 1）。

```sh
export SELFDB_ENV_API_KEY=... PYTHONPATH=packs:sdk/python
python3 -m packkit build packs/data http://env-node:8650 <template-id> --per-family 20 --out tasks.json --references refs.json
python3 -m packkit check http://env-node:8650 tasks.json refs.json
```

- `build`：并行校验候选（每个候选一个 rollout 判起始状态、一个 rollout 写入解答后判分），保留起始合格且解答得 1 分的任务，其余删除。每个 family 最多尝试数量的 20 倍。数量不足时退出码为 1，并列出不足的 family。
- `check`：每个 family 取一个任务，验证解答得 1 分、每个作弊用例得 0 分、只读路径拒绝写入。
- `tasks.json` 列出任务；`refs.json` 保存解答与作弊用例，供训练方评估使用，不属于任何任务。
- `judgelib.py`：判分常用函数（输出 verdict、数值容差比较、表格与 JSON 的部分得分、相对误差得分），以判分专用文件下发：`{"path": "/judge/judgelib.py", "content": packkit.judgelib()}`。
- `host-rootfs.sh OUT.tar PKG...`：无网络时，从 Debian/Ubuntu 宿主已安装的软件包打包模板 rootfs（含依赖闭包；alternatives 只取 rootfs 内的候选，不带入宿主的本地配置）。

实现注意：family 的迭代器应由生成器函数产生。在循环中写生成器表达式会在之后才读取循环变量，所有 family 都会生成最后一个 family 的任务；`build` 遇到 family 不符的候选会报错。
