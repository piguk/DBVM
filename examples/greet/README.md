# greet / libgreet.so.1 演示

```sh
make                              # 生成 libgreet.so.1、libgreet.so、libself-audit.so、app、app.self
LD_LIBRARY_PATH=. ./app           # 正常运行
bash demo.sh                      # 完整三段式（删 .so、closure 查询、bundle 自包含）
bash interpose.sh                 # LD_PRELOAD 演示
```

`make` 产物（`app`、`*.so*`、`app.self`）已加入 `.gitignore`，不入库。

bundle 自包含运行：

```sh
../../target/release/elf2self ./app -o /tmp/greet.bundle.self --bundle
LD_LIBRARY_PATH="" ../../target/release/self-exec /tmp/greet.bundle.self
```

`self-exec` 检测到 `bundle_objects` 时把 `.so` 展开到 `$TMPDIR/self-bundle-XXXXXX` 再执行。`libself-audit.c` 为 rtld-audit 桩，见 `docs/rtld-audit.md`。
