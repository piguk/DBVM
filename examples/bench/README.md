# bench / size

原文 §Cost & Benchmark 测得：单文件 SELF 约 2× ELF，批量 userland（723 executables -> 1123 objects）摊薄后仅比 ELF 大约 6%，且 `DELETE FROM sections/notes; VACUUM;` 可回收可选表开销。

本地可用 `examples/bench/size.sh` 与 `examples/bench/userland.sh` 复现单文件与 userland 趋势：

```
bash examples/bench/size.sh
bash examples/bench/userland.sh /tmp/userland.db /bin
```

`examples/bench/size.sh` 输出示例（含 bundle）：

```
ELF  158632 bytes
SELF 217088 bytes
BUNDLE 3584000 bytes  (bundle_objects=6)
bundle: 6 objects, 9 edges
/tmp/ls.bundle.self: bundle_objects=6 bytes=3341008 needs=9 self_size=3584000
SELF stripped  172032 bytes
```

`--bundle` 自包含时自身体积为主可执行文件 + 闭包 `.so` 的 `content` 之和（含原文图中的 `ld.so`）；单一 `ls` 闭包约 6 objects（`ld.so/libselinux/libcap/libc/libpcre2`），瘦身策略为宿主字节一致的系统库落盘跳过（`/tmp/self-bundle-XXXXXX` 仅含私有库如 `libgreet.so.1` 时约 15 KiB），空闭包目录会被回收不污染 `LD_LIBRARY_PATH`。

`scan` / `userland` 已会在聚合库上输出与原文一致的 headline 查询（distinct soname、top sonames、unresolved needs）。
动态与多对象场景的时延未在本 demo 中定量复现。
`vm` (新增)：`self vm-init/vm-import/vm-exec` 为单文件 VM，`vm-exec` 按需 materialize 到 `mkdtemp`，`--version` 约 6.6 ms（见 README 单文件 VM 一节）。
