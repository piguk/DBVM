# closure / 去重

- `objects(id, path, soname, kind, is_root)` 与 `needs(object_id, ord, soname, resolved_path)`；`resolved_path` 引用 `objects.path`，消除 `ldd` 歧义。
- `self closure <elf> <out.db>`：打包单根及传递 `DT_NEEDED` 闭包，`missing=0` 为成功。
- `self scan <db> <dir>` / `self userland <out.db> <dir...>`：多 ELF 聚合去重。
- `elf2self --bundle`：同库写入 `bundle_objects(content)` + `bundle_needs`；`self bundle` / `self bundle-info` 列出。

寻库顺序（每个对象）：`RUNPATH`（`$ORIGIN` 展开）→ `LD_LIBRARY_PATH` → `RPATH` → 调用方目录 → 系统目录 `/lib*`, `/usr/lib*`。`RUNPATH` 存在时忽略 `RPATH`，与 glibc 一致。
