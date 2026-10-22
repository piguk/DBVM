# binfmt_misc 注册（未验证）

SELF 文件：偏移 0 为 SQLite 头 `SQLite format 3\0`，偏移 68..71 为 `application_id = 0x53454C46` (`SELF`)。

binfmt 的 magic 必须是连续字节串，且 mask 与 magic 等长。"偏移 0 的 16 字节 + 偏移 68 的 4 字节"需要 52 字节间隔，直接用 `:offset:magic:mask` 表达不了；需要 `magic` 带 52 字节通配（mask 为 `\x00`）。此格式未在本机验证，注册前请先用 `/proc/sys/fs/binfmt_misc` 试验。

`self-exec` 必须是普通 ELF，否则会递归触发 `ELOOP`。
