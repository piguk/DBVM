# self-exec

SELF 加载器。`self-exec <file.self> [args...]`。

- 从 `segments` 重建 ELF 写入 `memfd`，`execve(/proc/self/fd/N)` 交给宿主 `ld.so`（静态与动态同一路径）。
- 若存在 `bundle_objects`：私有库按内容哈希（sha256）存入共享缓存 `cache_dir()/bundle/<hash>`，每次启动只在 `$TMPDIR/self-bundle-XXXXXX` 中建立指向缓存的链接，并前置 `LD_LIBRARY_PATH`。缓存项按存储的大小校验，不一致则重建；运行结束只删除链接目录，不删缓存。

不做：DB 内纯 `self-ld` 链接、`mmap` 权限细化、IRELATIVE 自行解析（由 `ld.so` 完成）。
