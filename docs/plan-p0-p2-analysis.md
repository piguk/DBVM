# P0/P1/P2 计划分析与实施方案

结论先行：**P0 第 1 项的前提不成立**。owner + mount_link_text 只是必要条件的两个，真正阻塞的是挂载层会丢失「父目录为符号链接」的路径。这一条不修，`bwrap --bind <fuse_mount> /` 无法取代物化。

P1 的两条**都已实测确认为真 bug**，其中不只是一致性问题，而是**静默数据丢失与错误语义**。

---

## 一、已实测确认的事实（可复现）

### 1.1 P1.2 重命名丢失文件内容（数据丢失）

```
$ mv $M/data/f.txt $M/data/g.txt   # 成功
$ cat $M/data/g.txt
cat: .../g.txt: Input/output error     # 读不出来
$ sqlite3 v.db "SELECT path FROM vm_fs WHERE path LIKE '/data/%'"
/data/g.txt                            # vm_fs 已改名
$ sqlite3 v.db "SELECT path,count(*) FROM vm_file_chunks WHERE path LIKE '/data/%' GROUP BY path"
/data/f.txt|1                          # chunks 仍在旧路径
```

`vm_file_chunks` 无 `ON UPDATE CASCADE`（schema 已核对，`PRIMARY KEY(path,idx) WITHOUT ROWID`），`rename` 只 `UPDATE vm_fs SET path`。后果：重命名后的文件**内容永久不可读**，且不是空文件，而是 EIO。

### 1.2 P1.1 策略保护下 rm 假成功（错误语义 + 视图/库分歧）

```
$ rm -f $M/tests/eval.sh ; echo $?
0                                      # 报告删除成功
$ test -e $M/tests/eval.sh; echo $?
1                                      # 挂载视图里也没了
$ sqlite3 v.db "SELECT path FROM vm_fs WHERE path='/tests/eval.sh'"
/tests/eval.sh                         # 数据库里还在
```

重挂后文件"复活"，内容完好。用户视角：删不掉（策略生效），但 `rm` 说成功、视图里消失、重挂又回来。`unlink`/`rmdir`/`rename`/`setattr` 全部丢弃 SQL 结果后无条件 `reply.ok()`。已核对，共 6 处：
`fuse.rs:427`（unlink DELETE）、`451`（rmdir DELETE）、`509`（rename 覆盖目标 DELETE）、
`530`（rename UPDATE path）、`544`（setattr chmod）、`562`（setattr utimens）。

另外这些路径也没有 `policy::is_refusal → EACCES` 映射，只有 `write` 有。

### 1.3 P0.1 的阻塞点：挂载丢失「父目录是符号链接」的路径

真实导入的库（`vm-import /bin/sh`）结构：

```
/bin -> /usr/bin
/bin/sh -> /usr/bin/dash
```

挂载后：

```
$ find $M -mindepth 1
/bin  /lib  /lib64  /usr  /usr/bin  /usr/bin/dash  ...   # 没有 /bin/sh
$ ls $M/usr/bin
dash                                                       # 也没有 sh
$ bwrap --bind $M / ... /bin/sh -c 'echo OK'
bwrap: execvp /bin/sh: No such file or directory
```

同一库 `vm-materialize` 后：

```
$ find $M2 -mindepth 1
... /usr/bin/dash  /usr/bin/sh      # 别名落在真实父目录下
$ bwrap --bind $M2 / ... /bin/sh -c 'echo OK'
MATERIALIZE_SH_OK
```

根因：`vm_materialize_tree` 在 agent-step 阶段修过此问题（把子项写到父链接解析后的真实目录），**FUSE 的 `rebuild` 没有对应逻辑**，它按字面 DB 路径建 inode。`/bin` 是符号链接，于是 `/bin/sh` 这个 inode 不被任何目录列举到，等于不可达。

这是 P0.1 的硬前提。owner/uid 与 link 改写解决的是"权限"和"逃逸"，不解决"路径可达"。

### 1.4 「策略拦截滞后」实测：篡改被静默丢弃，步骤仍报成功

环境：DB 内有 `/etc/app.conf`（内容 `orig`），策略 `readonly_prefix /etc`，执行
`agent-step -- /bin/sh -c 'echo tampered > /etc/app.conf'`。

```
exit=0  stdout="WRITE_DONE\n"  stderr=""  created=[]  modified=[]
db_content="orig"      # 篡改没有发生
```

沙箱内写入成功、无任何报错、步骤退出码 0，数据库内容未变，变更清单为空。调用方无法得知写入被拒绝。

机制（已核对代码）：`sync_report_impl` 对单个文件的写入错误只做
`Err(err) => { eprintln!("sync {}: {}", ..); rep.failed += 1; }`，函数末尾仍返回 `Ok(rep)`；
`agent::run_step` 用 `?` 只能捕获函数级错误，因此拿不到 `failed` 计数。
而 `run_step` 返回的 `stderr` 只含沙箱命令输出，agent 自身的 `eprintln!` 不在其中。

对比参照：同样的写入走 `vm-add`（直接落库）时策略当场拒绝并返回
`Error code 1811: A RAISE function within a trigger fired`。所以策略本身有效，
问题在 `agent-step` 丢掉了失败信号。

结论：P0.1 切到"直写挂载"确实能修掉这一条（写入当场失败，命令能看到 EACCES），
但**即使不切**，这条也应该先单独修：`run_step` 必须检查 `rep.failed > 0` 并使步骤失败。

---

## 二、计划逐条评估

### P0.1 让 run_step 直接跑在 VmFuse 挂载点上

| 项 | 评估 |
|---|---|
| 「替掉 materialize + sync 两次全量 I/O」 | 方向正确。实测该库 `vm_fs` 有 16 行，物化后为 19 个条目（多出的 `/dev`、`/proc`、`/sys`、`/tmp` 是物化层补的，挂载层不补）。真实 rootfs 是数万条，每次 step 都扫两遍代价明显。 |
| 「策略拦截滞后」 | 成立，且比原描述更严重，已实测（见 1.4）：拦截发生在 sync 回写，错误被记为 `failed` 计数而**不使步骤失败**。 |
| 「owner + link 已扫清最后障碍」 | **不成立**，见 1.3。另有三处前置：|
| 前置 A：路径可达 | `rebuild` 需与 materialize 同一套"父链接展开"逻辑。 |
| 前置 B：FUSE 写入端到端未验证 | 目前只有库级 `is_refusal` 测试；挂载写入从未在自动化测试中跑过（本机 uid 限制）。P0.1 等于把主执行链压在这条未验证路径上，风险高。 |
| 前置 C：`/dev`、`/proc` | 已实测：物化 16 行 DB 得到 19 个条目，多出的 `/dev`、`/proc`、`/sys`、`/tmp` 是 `vm_materialize_tree` 补的，挂载层不补。以挂载为根时这些路径由 bwrap 的 `--dev`/`--proc` 提供，但 `/tmp` 等目录仍需存在，否则依赖它的程序会失败。 |

**建议顺序**：先修 1.3（A），再补一个能在 CI 跑的挂载写入测试（B），最后才切 run_step。切的时候保留 `--materialize` 回退开关。

### P0.2 Overlay 接入 CLI / agent-step / VmFuse

| 项 | 评估 |
|---|---|
| CLI `--base` | 低风险，价值高。`Overlay::open` 已成熟。 |
| 快照/回滚覆盖 `vm_whiteouts` | **确认缺口**：`vm_snapshot_fs`/`vm_snapshot_chunks` 不含 whiteout（schema 已核对）。Delta 内回滚若不恢复 whiteout，会把已删除的 Base 路径重新暴露，属于正确性问题。 |
| 跨库块级 COW | 方向可行：`decode_block_in(schema)` 已支持按 schema 解码，inject 已证明跨库 INSERT SELECT 可用。但要注意 **1.2 的教训**：跨库复制 chunk 引用时 refcnt 与删除顺序极易出错（我在 inject 中踩过两次：id 冲突、refcnt=0 被清理）。必须有 `vm_verify --deep` 兜底。 |

### P1 细节补齐

两条都会被我按 1.1/1.2 的方式先写失败测试，再修。注意 P1.1 不只是"映射 EACCES"：**必须让失败时内存字典不变**，否则视图与库继续分歧。

### P2 性能与基准

前提是 P0/P1 完成。注意本仓库已有基准经验：整文件重写 2616ms → 块级 66ms，且 66ms 里约 30ms 是不可省的整文件 sha256。做 COW 基准时要说明这一点，否则会误判收益。

---

## 三、实施方案

### 第一步（P1，先修正确性，不动架构）

优先级理由：这两条是当前**已存在的数据丢失/错误语义**，与架构改造无关，且改动面小。

1. `rename`：在 SAVEPOINT 内同时更新 `vm_file_chunks.path`（按路径前缀批量），失败整体回滚。
2. `unlink`/`rmdir`/`rename`/`setattr`：不再丢弃 `execute` 结果；失败时**不改内存字典**，返回 `EACCES`（策略拒绝）或对应 errno。
3. 全部写回调统一走一个错误映射函数，避免以后再漏。

每项：先写能复现的失败测试（挂载端到端，用 `unshare -r`），修完在同一测试上验证通过，再做变异。

### 第二步（P0.1 前置）

1. `rebuild` 与 `vm_materialize_tree` 共用"父链接展开"逻辑：抽成一个函数，两处都调用，保证挂载与物化的树形一致。
2. 挂载内补 `/dev`、`/proc`、`/tmp` 等 bwrap 必需的空目录（或说明必须由 bwrap 提供）。
3. 新增可在 CI 跑的挂载写入测试（`unshare -r` 内挂载 + 写入 + 读回），覆盖 write/create/mkdir/unlink/rename/truncate。

### 第三步（P0.1 本体）

`run_step` 增加 `--fuse`（默认关闭），路径：挂载 → bwrap 绑定 → 直写 → 卸载 → `snapshot_diff_live`。失败回退到现有物化路径。对比两者的步骤耗时与 `vm_verify --deep` 结果。

### 第四步（P0.2）

按 CLI `--base` → whiteout 快照 → 块级 COW 的顺序，每步独立可验证。

---

## 四、需要你确认的点

1. P1 的两条我先直接修（明确 bug），是否同意？
2. P0.1 是否按"先修路径可达 + 补挂载写入测试，最后才切 run_step"的顺序推进？
3. `/dev`、`/proc` 由挂载提供还是继续由 bwrap 提供（影响 FUSE 是否需要伪造这些目录）？
