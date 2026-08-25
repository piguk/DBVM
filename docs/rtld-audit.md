# rtld-audit 方案

原文 Dynamic linking 节的两条路线之一：保留 `ld.so`，用 `glibc rtld-audit` 的 `la_objsearch` 拦截寻库，改为 SQL 查询（`LD_AUDIT=libself-audit.so` + `SELF_SYSTEM_DB=system.db`）。

此路径下 `ld.so` 仍负责 `mmap / relocate / TLS / IFUNC / lazy PLT / symbol versioning`，库文件在磁盘上可被删除，仅以数据库中的行存在。

提供最小桩 `examples/greet/libself-audit.c`（`la_objsearch` 从 `needs` 查 `resolved_path`）与可运行示例 `examples/greet/demo.sh`。
stock `ld.so` 仍负责重定位/TLS/IFUNC，删掉 `.so` 后若 `SELF_SYSTEM_DB` 仍指向 closure 则审计回调可替代文件系统寻库（stub 版本仅作演示，返回 `needs` 中记录的路径）。
