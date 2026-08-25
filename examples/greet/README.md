# greet / libgreet.so.1 demo (article §Dynamic linking)

复现原文的三段式演示：

1. 正常动态链接：`make && LD_LIBRARY_PATH=. ./app` 输出 `Hello, world, from a SQLite library!`
2. 删掉 `.so` 后失败：`mv libgreet.so.1 libgreet.so.1.bak && LD_LIBRARY_PATH=. ./app` 报错 `cannot open shared libraries`
3. closure 仍可查询：`dbvm self closure ./app /tmp/greet_closure.db` 后 `SELECT ... FROM needs` 可看到 `libgreet.so.1 -> /.../libgreet.so.1`
4. 若有 `LD_AUDIT=libself-audit.so`（`la_objsearch` 桩在 `libself-audit.c`），则 `SELF_SYSTEM_DB=/tmp/greet_closure.db LD_AUDIT=./libself-audit.so ./app` 会在不依赖文件系统寻库的情况下由 SQL 回答寻库，`ld.so` 仍负责重定位/TLS/IFUNC。

5. 自包含 bundle（无需原 `.so` 即可运行，`--bundle`）：

   ```sh
   cargo run --release --bin elf2self -- ./app -o /tmp/greet.bundle.self --bundle  # 或 target/release/elf2self
   LD_LIBRARY_PATH="" ../../target/release/self-exec /tmp/greet.bundle.self   # 删原 .so 后仍通过
   sqlite3 /tmp/greet.bundle.self "SELECT soname,path,kind,size FROM bundle_objects; SELECT soname,resolved_path FROM bundle_needs;"
   ```

   `self-exec` 检测到 `bundle_objects` 时将闭包 `.so` 的 `content` 展开到 `mkdtemp(/tmp/self-bundle-XXXXXX)` 并前置 `LD_LIBRARY_PATH` 后再 `execve`，展开目录在 `/tmp` 留存。

`libself-audit.c` 仅为桩，说明 audit 方案保留 `ld.so` 的分工；bundle 仍经宿主 `ld.so`，非纯 `self-ld`。
