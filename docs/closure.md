# closure / one file one userland

`objects(id, path, soname, kind, is_root)` + `needs(object_id, ord, soname, resolved_path)` 与原文一致：`resolved_path REFERENCES objects(path)` 是消除 `ldd` 歧义的 FK。

- `dbvm self closure <elf> <out.db>` 打包单个根及其传递 `DT_NEEDED` 闭包（见 `src/bin/dbvm.rs:build_closure`），用 `resolved_path` 去重并验证 `missing=0`。
- `dbvm self scan <db> <dir>` / `dbvm self userland <out.db> <dir...>` 会递归展开库的依赖并以 `DT_SONAME` 填充 `objects.soname`，用 `/bin` 已可得到 数千 objects（示例中 per-object 展开后 unresolved needs=0） 的聚合库，`examples/bench/userland.sh` 输出与原文 headline queries 一致的去重统计。
- `--bundle` 自包含：`elf2self --bundle` 在同库中写入 `bundle_objects(content BLOB)` + `bundle_needs`，`dbvm self bundle / dbvm self bundle-info` 列出嵌入的闭包，`self-exec` 在检测到 `bundle_objects` 时将 `lib` 的 `content` 展开到 `mkdtemp(/tmp/self-bundle-XXXXXX)` 并前置 `LD_LIBRARY_PATH`，删原 `.so` 后仍可运行。本文 demo 中 `greet.bundle` 为 4 objects，`ls.bundle` 为 6 objects（含 `ld.so`）。

单文件 SELF 约 2× ELF（如 `/bin/ls` 158K→217K），聚合后 b-tree 摊薄（原文 611.9 vs 644.4 MiB），`examples/preload.sql` 中 `preload` 表以事务原子地开关 `LD_PRELOAD`。
