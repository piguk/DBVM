# EDA 环境包

RTL、FPGA 时序与模拟电路三类任务，用于训练和评估做芯片设计相关工作的 agent。工具为 Verilator（RTL 仿真）、Yosys 与 nextpnr（FPGA 综合与布局布线，来自 YoWASP，即编译为 WebAssembly 的版本，pip 安装、无需 root）和 ngspice（电路仿真），均为开源。

## 任务

| 类别 | agent 的工作 | reward |
|---|---|---|
| RTL（8 个设计：alu、counter、fifo、lfsr、prio_enc、gray、edge_detect、shift_reg） | 参考设计中被注入一处错误（运算符、常数、时钟沿、条件、取反），修复 `/work/rtl/<name>.v` | 自检 testbench 的通过比例（每个设计 300–600 次检查，随机激励由 LFSR 产生，可复现） |
| FPGA 时序（dot4、sumsq4、max8，位宽随机） | 模块单周期算出结果，限制了 iCE40 HX8K 上的时钟；插入流水线提高时钟，接口与流式行为不变（每个 in_valid 一个结果，按输入顺序，最多晚 16 个周期），逻辑单元不超过初始的 2 倍 | testbench 必须全部通过；之后在包装模块（所有端口打一拍）中综合、布局布线（固定 seed），按时钟在"初始频率"与"流水线参考解的 95%"之间线性给分；超出面积预算为 0 |
| 模拟（rc_lowpass、divider、rlc_bandpass、inverting_amp） | 调整 `/work/circuit.cir` 中子电路 `dut` 的元件取值，使指标达到随机给定的目标 | 各指标得分的最小值：容差内为 1，偏离到 `zero_at` 倍时为 0；约束类指标（电流上限、输入电阻下限、峰值增益）为 0 或 1 |

每个任务的文件（待修改的设计、只读的 testbench 或测量 deck、判分脚本）以任务文件（`files`）的形式提供，所有任务共用一个工具模板。`/tests` 只读；判分在新的 sandbox 中运行，只采用 agent 在 `/work` 下的改动。

### 判分对作弊手段的处理

- RTL：去掉注释后，只允许一个模块（名称固定）；除 `$signed`、`$unsigned`、`$clog2` 外不允许系统任务和函数（否则模块可以自己打印 testbench 的结果行）；不允许 `initial`、`final`、编译指令。
- 模拟：子电路只能包含规格允许的元件类型（不允许电源和受控源，否则可以把输出固定在目标值）；不允许参数、表达式和指令；元件值必须在规定范围内。

- FPGA：同 RTL 的规则；另有"流水线化但结果错误"的用例，testbench 不通过即为 0。

`packkit check` 对每类任务验证参考解得 1 分、上述手段得 0 分、`/tests` 拒绝写入。

## 使用

```sh
# 1. 工具模板：有网络时
docker build -t selfdb-eda packs/eda && docker save selfdb-eda -o eda.tar
self env-admin --data /srv/env image eda eda.tar --oci
#    无网络、宿主为 Debian/Ubuntu 且已安装这些包时
packs/packkit/host-rootfs.sh eda-rootfs.tar verilator ngspice g++ make perl python3
self env-admin --data /srv/env image eda eda-rootfs.tar

# 2. 生成并校验任务（起始状态得分 < 1，参考解得 1 分；不满足的丢弃）
export SELFDB_ENV_API_KEY=... PYTHONPATH=packs:sdk/python
python3 -m packkit build packs/eda http://env-node:8650 <template-id> --per-family 5 --out tasks.json --references refs.json

#    FPGA 任务：模板需要 YoWASP，生成任务时在宿主上用 YoWASP 测出初始与目标时钟
pip install --target /opt/yowasp yowasp-yosys yowasp-nextpnr-ice40
YOWASP=/opt/yowasp packs/eda/fpga-template.sh eda-fpga.tar && self env-admin --data /srv/env image eda-yowasp eda-fpga.tar
python3 packs/eda/warm_fpga.py http://env-node:8650 <template-id>      # 预热 YoWASP 的编译缓存并 freeze，打印新模板 id
YOWASP=/opt/yowasp python3 -m packkit build packs/eda http://env-node:8650 <warmed-template-id> --families fpga/ --per-family 5

# 3. 检查判分
python3 -m packkit check http://env-node:8650 tasks.json refs.json
```

`tasks.json` 列出任务 id、family（如 `rtl/alu`、`analog/divider`）、指令和起始得分；`refs.json` 是参考解与作弊用例，供训练方评估使用，不属于任何任务。生成与校验由 `packs/packkit` 完成（见其说明）。

## 实测（本机，16 核；release 构建）

| 项目 | 结果 |
|---|---|
| 工具 rootfs（144 个包，宿主打包） | 645 MB；导入 94 s；模板层 644 MB + 数据库 252 MB |
| 生成并校验 12 个任务（每个设计 / 电路类型 1 个，8 个 worker） | 77 s，淘汰 1 个候选 |
| RTL 判分（编译 + 仿真） | 约 4 s |

## 限制

- `Dockerfile`（Alpine）未在本机构建：本机没有网络。本机验证使用 `packs/packkit/host-rootfs.sh` 打包的 Debian rootfs。
- 时序与面积指标来自 iCE40（Yosys + nextpnr）。ASIC 流程（OpenROAD、OpenSTA、PDK）没有：本机无法安装。
- 变异只改动一处；等价变异（testbench 区分不出来）在校验时被丢弃。
