# 数据分析任务包

一家公司的数据（按 seed 生成：客户、长尾分布的产品、约 2000 个订单）上的问题，答案为 CSV 表或 JSON 报表。模板只需 python3（含 sqlite3 模块）。

| family | 内容 | 答案 |
|---|---|---|
| `sql/revenue_by_region` | 某年各地区营收，降序 | `/work/answer.csv` |
| `sql/top_customers` | 某年营收最高的 k 位客户 | `/work/answer.csv` |
| `sql/unordered_products` | 某类目中当年没有完成订单的产品 | `/work/answer.csv` |
| `report/yearly` | 年度报表：订单数、营收、营收最高的月份、客单价 | `/work/report.json` |
| `csv/category_quantities` | 只给 CSV 文件，按类目汇总销量 | `/work/answer.csv` |

- 数据在 `/data`（只读）：SQLite 数据库或 CSV 文件，作为任务文件下发。
- 期望结果由生成器按参考 SQL 计算，与判分脚本一起作为**判分专用文件**下发，rollout 中看不到。
- 判分：CSV 表头必须一致，否则为 0；之后按行比较（数值有容差），得分为匹配行数 / max(提交行数, 期望行数)；要求排序的题按位置比较。JSON 按字段比较，得部分分。
- 作弊用例（得 0）：空文件、只有表头、把答案写到别处。

```sh
packs/packkit/host-rootfs.sh python.tar python3      # 或 docker build packs/data
self env-admin --data /srv/env image python python.tar
PYTHONPATH=packs:sdk/python python3 -m packkit build packs/data http://env-node:8650 <template-id> --per-family 20
```
