# SELF：把 ELF 装进数据库

把 ELF 转换为 `application_id = 0x53454C46 ("SELF")` 的 SQLite 数据库：段、符号、节、notes、
动态表都是可查询的行，`self-exec` 负责把它重新装回内存执行。

相关文档：[self-exec](self-exec.md) 加载器、[closure](closure.md) 依赖闭包、
[rtld-audit](rtld-audit.md) 保留 `ld.so` 的另一条路线、[binfmt](binfmt.md) 注册为可执行格式。

## 转换与查询

```sh
# 静态
gcc -static -no-pie -o /tmp/hello-static examples/hello_static.c
elf2self /tmp/hello-static -o /tmp/hello.self
dbvm self file /tmp/hello.self
dbvm self ldd  /tmp/hello.self
dbvm self meta /tmp/hello.self
sqlite3 /tmp/hello.self "SELECT sql FROM sqlite_master"

# SQL 代替 readelf/nm/ldd
dbvm self exports  /tmp/ls.self | head
dbvm self imports  /tmp/ls.self | head
dbvm self segments /tmp/hello.self
sqlite3 /tmp/hello.self < examples/queries.sql
sqlite3 /tmp/hello.self < examples/strip.sql   # DELETE + VACUUM，仍可运行
```

## bundle：自包含

`--bundle` 把依赖闭包的 `.so` 内容一并写进同一个库（`bundle_objects` + `bundle_needs`），
删掉原始 `.so` 后仍可运行。

```sh
elf2self /bin/ls -o /tmp/ls.bundle.self --bundle
dbvm self bundle      /tmp/ls.bundle.self
dbvm self bundle-info /tmp/ls.bundle.self
LD_LIBRARY_PATH="" self-exec /tmp/ls.bundle.self --version
```

## 运行

`self-exec` 统一经 `memfd` + `execve` 委托宿主 `ld.so`，静态与动态同一路径。

```sh
dbvm self run /tmp/hello.self
self-exec /tmp/hello.self       # 等价，直接调用加载器
```

## 闭包与 userland 聚合

```sh
dbvm self closure /bin/ls /tmp/coreutils.db
sqlite3 -column /tmp/coreutils.db \
  "SELECT n.soname, n.resolved_path FROM needs n JOIN objects o ON o.id=n.object_id WHERE o.is_root=1"

dbvm self scan /tmp/scan.db /bin
dbvm self userland /tmp/userland.db /bin /usr/bin
bash examples/bench/userland.sh /tmp/userland.db /bin
```

动态库示例（`examples/greet/`，复现删除 `libgreet.so.1` 后的三段行为）：

```sh
make -C examples/greet
bash examples/greet/demo.sh
bash examples/greet/interpose.sh
```
