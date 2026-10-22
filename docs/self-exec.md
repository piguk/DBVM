# self-exec

SELF 加载器。`self-exec <file.self> [args...]`。

- 从 `segments` 重建 ELF 写入 `memfd`，`execve(/proc/self/fd/N)` 交给宿主 `ld.so`（静态与动态同一路径）。
- 若存在 `bundle_objects`：把 `.so` 内容展开到 `$TMPDIR/self-bundle-XXXXXX`，前置 `LD_LIBRARY_PATH` 后再执行；删原 `.so` 仍可运行。展开目录不自动删除。

不做：DB 内纯 `self-ld` 链接、`mmap` 权限细化、IRELATIVE 自行解析（由 `ld.so` 完成）。
