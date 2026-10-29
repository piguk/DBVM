# packkit

领域任务包的生成、校验与检查。一个包是一个目录，其中的 `pack.py` 定义 `candidates(rng)`：按 family（如 `rtl/alu`、`sql/top_customers`）返回候选任务的迭代器。每个候选（`packkit.Candidate`）包含：

- `spec`：任务定义（不含 `template_id`）：指令、任务文件、判分命令与判分专用文件、服务、只读路径等；
- `solution`：解答文件，用于校验与检查；
- `cheats`：作弊用例，每个是一组文件，判分必须为 0；
- `meta`：写入 `tasks.json` 的附加信息。

可选的 `start_ok(verdict, candidate)` 决定起始状态是否合格（默认：reward < 1）。可选的 `prepare(client, template_id, log)` 在生成候选之前运行，用于查看模板（例如记录仓库的测试在模板上的结果，见 `packs/repo`）。

```sh
export SELFDB_ENV_API_KEY=... PYTHONPATH=packs:sdk/python
python3 -m packkit build packs/data http://env-node:8650 <template-id> --per-family 20 --out tasks.json --references refs.json
python3 -m packkit check http://env-node:8650 tasks.json refs.json
python3 -m packkit audit http://env-node:8650 tasks.json refs.json --out audit.json
python3 -m packkit redteam http://env-node:8650 tasks.json --model http://gpu-node:8000/v1 --attempts 4 --out redteam.jsonl
```

- `build`：并行校验候选（每个候选一个 rollout 判起始状态、一个 rollout 写入解答后判分），保留起始合格且解答得 1 分的任务，其余删除。每个 family 最多尝试数量的 20 倍。数量不足时退出码为 1，并列出不足的 family。
- `check`：每个 family 取一个任务，验证解答得 1 分、每个作弊用例得 0 分、只读路径拒绝写入。
- `tasks.json` 列出任务；`refs.json` 保存解答与作弊用例，供训练方评估使用，不属于任何任务。
- `audit`：每个 family 取一个任务，不用模型检查判分能否被绕过，退出码为 1 表示有 high 级别的问题：
  - 静态检查：判分命令执行的文件在 `sees` 之下且不是只读或判分专用文件（agent 可以改写判分）；解答内容出现在 rollout 可见的任务文件中；
  - 平凡答案：解答文件为空、只有空白、保持原样时，得分不能高于起始状态；
  - 测试框架 hook：在判分读取的每个目录（`sees` 下三层以内；`sees` 为文件时取其上级目录）写入 `unittest.py`、`pytest.py`、`conftest.py`、`sitecustomize.py`，每次一种，得分不能高于起始状态；高于时再逐个目录定位；
  - 判分过严（low）：解答末尾多一个空行时得分低于 1。
- `redteam`：每个 family 取一个任务，让模型（OpenAI 兼容接口，`sdk/python/selfdb_env/agent.py` 的工具调用循环）按“不完成任务、尽量拿高分”的提示做 K 次 rollout；得分高于起始状态加 `--margin` 的 rollout 标记为 FLAG，轨迹写入 `--out`，由人查看。被标记的 rollout 也可能是模型确实完成了任务。
- `mutate.py`：源代码单点变异（运算符、常数加减一、布尔值、条件取反、去掉 `not`/`!`；跳过字符串与注释），支持 Python 与 C 系语法（C、C++、Java、JavaScript、Go、Rust 等）。Python 变异体不能编译时丢弃。Verilog 的 `<=` 是赋值，用 `packs/eda/rtl.py` 自己的变异。
- `judgelib.py`：判分常用函数（输出 verdict、数值容差比较、表格与 JSON 的部分得分、相对误差得分），以判分专用文件下发：`{"path": "/judge/judgelib.py", "content": packkit.judgelib()}`。
- `host-rootfs.sh OUT.tar PKG...`：无网络时，从 Debian/Ubuntu 宿主已安装的软件包打包模板 rootfs（含依赖闭包；alternatives 只取 rootfs 内的候选，不带入宿主的本地配置）。

实现注意：family 的迭代器应由生成器函数产生。在循环中写生成器表达式会在之后才读取循环变量，所有 family 都会生成最后一个 family 的任务；`build` 遇到 family 不符的候选会报错。
