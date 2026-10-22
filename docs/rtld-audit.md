# rtld-audit 方案（实验）

用 glibc `LD_AUDIT` 的 `la_objsearch` 替代文件系统寻库：`ld.so` 仍负责 mmap/重定位/TLS/IFUNC，只把"找哪个 .so"改为查 SELF closure。

- 桩：`examples/greet/libself-audit.c`，从 `SELF_SYSTEM_DB` 的 `needs.resolved_path` 返回路径。
- 运行：`SELF_SYSTEM_DB=closure.db LD_AUDIT=./libself-audit.so ./app`（`bash examples/greet/interpose.sh` 为演示）。

仅为概念验证；未做错误处理，不支持 `dlopen` 动态加载路径。
