# self-exec

最小 SELF 解释器（对标原文的 `self-exec`，角色等价于 `ld.so` 的精简版）。

- 统一经 `memfd`：从 `segments` 按 `offset` 重建 ELF 至 `memfd`/`tmp` 并 `execve(/proc/self/fd)` 委托宿主 `ld.so`（透传 `argv`/`envp`，含 `LD_LIBRARY_PATH`）；静态与动态统一走此路径（历史静态 `mmap+auxv` 直跳实现见 git 历史）。
- 支持 `elf2self --bundle` 的自包含执行：`self-exec` 检测到 `bundle_objects` 时先将闭包 `.so` 的 `content` 展开到 `mkdtemp(/tmp/self-bundle-XXXXXX)` 并置于 `LD_LIBRARY_PATH` 前缀，使删除原 `.so` 后仍可运行；展开目录随进程生命周期驻留 `/tmp`。系统库字节一致时跳过落盘、空目录自动回收。

未覆盖：纯 DB 内 `self-ld` 无落盘、`mmap` 细粒度权限还原与 `R_X86_64_IRELATIVE` 直解（现由 `ld.so` 完成）。
