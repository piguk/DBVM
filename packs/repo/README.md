# 仓库任务包

从客户自己的仓库生成任务，以仓库已有的测试判分。两种方式：

| family | 起始状态 | 解答 |
|---|---|---|
| `<name>/mutation` | 源文件中一处单点变异（`packkit/mutate.py`：运算符、常数、布尔值、条件取反） | 原文件 |
| `<name>/history` | 过去某个提交对源文件的改动在 HEAD 上被撤销（三方合并；相邻改动按 hunk 定位），其后的改动保留 | HEAD 的文件 |

- 只保留测试能发现的任务：起始状态得分必须低于 1，解答得 1（`packkit build` 的校验）。测试发现不了的变异体和提交被丢弃。
- 跳过的提交：根提交、merge、新增或删除源文件的提交、撤销时冲突的提交。
- 指令：变异任务给出文件名；历史任务默认给出提交信息（`"history": {"message": false}` 时只说有测试失败）。指令中列出 agent 可以运行的测试命令（`agent_cmd`）。

## 模板与配置

模板中需要有 python3（判分脚本），仓库放在 `root`，内容与 `checkout` 的 HEAD 一致；隐藏测试不放进模板（例如用 `git archive HEAD ':!tests/hidden'` 导出）。`packkit build` 先运行 `prepare`：在一个 rollout 中比较模板与 HEAD 的源文件、确认隐藏文件不在模板中，再在模板上运行各项检查，记录 HEAD 上通过的测试。任务只计这些测试。

```json
{"name": "calc", "checkout": "../calc", "root": "/work/repo",
 "include": ["calc/**/*.py"], "exclude": ["**/__init__.py"], "hidden": ["tests/test_hidden.py"],
 "checks": [{"name": "unit", "cmd": "python3 /judge/unittest_junit.py -s tests -t . -o /tmp/unit.xml",
             "junit": "/tmp/unit.xml", "agent_cmd": "python3 -m unittest discover -s tests -t .", "timeout": 60}],
 "modes": ["mutation", "history"], "history": {"commits": 200}}
```

| 字段 | 含义 |
|---|---|
| `checkout` | 宿主上的 git 仓库（相对配置文件），读取 HEAD 与历史 |
| `include` / `exclude` | 源文件（agent 修改的文件），glob：`*` 不跨目录，`**` 跨目录 |
| `hidden` | 隐藏测试：作为判分专用文件下发到 `root` 下的原路径，rollout 中不可见 |
| `checks` | 判分命令，在 `root` 下运行。有 `junit`（路径或 glob）时，得分为 HEAD 上通过的测试中现在仍通过的比例；没有时，退出码为 0 得 1 分。总分为各项的加权平均（`weight`） |
| `lang` | `python` 或 `c`（C 系语法）；默认按扩展名判断 |
| `env` / `limits` / `instruction` | rollout 与判分的环境变量、资源限制、附加在指令后的文字 |

JUnit 报告：pytest 用 `--junitxml`，Gradle/Maven、`go-junit-report`、`ctest --output-junit` 等均可；unittest 用随包下发的 `/judge/unittest_junit.py`。

## 判分的边界

- 判分只取 rollout 中**已有源文件**的改动（`sees` 为这些文件）。测试、测试配置、agent 新建的文件（`conftest.py`、遮蔽测试框架的模块）都来自模板，不影响判分。
- 被测代码与测试在同一进程中运行，可以伪造结果。只看退出码的检查会被“导入时 `os._exit(0)`”一类的代码绕过；带 JUnit 时，提前退出没有报告，得 0 分。伪造一份含正确测试名的 JUnit 报告仍然可能，隐藏测试让测试名不可见。用 `packkit redteam` 检查。
- 记录 HEAD 上通过的测试时，测试 id 列表经判分输出返回，上限 1 MiB（约一万个测试）。

```sh
export SELFDB_ENV_API_KEY=... PYTHONPATH=packs:sdk/python REPO_PACK_CONFIG=repo.json
python3 -m packkit build packs/repo http://env-node:8650 <template-id> --per-family 50
python3 -m packkit check http://env-node:8650 tasks.json refs.json
python3 -m packkit audit http://env-node:8650 tasks.json refs.json
```
