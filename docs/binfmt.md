# binfmt_misc

SELF 文件头为 SQLite 头（`SQLite format 3\x00`，偏移 0）且 `application_id` 在偏移 68 处为 `SELF`（`0x53454C46`）。

`self.conf` 为 `systemd-binfmt` 注册示例，指向 `self-exec` 解释器（`target/release/self-exec`）。
`self-exec` 本身必须是 ELF，否则会递归触发 `ELOOP`。

注册后可直接 `chmod +x hello.self && ./hello.self`。

## self.conf

```ini
# systemd-binfmt registration for SELF (SQLite at 0, application_id SELF at 68)
# Place as /usr/lib/binfmt.d/self.conf then: systemctl restart systemd-binfmt
:SELF:M:0:SQLite format 3\x00:!:SELF:/usr/local/bin/self-exec:OC
# For NixOS (from article):
# boot.binfmt.registrations.self = {
#   recognitionType = "magic";
#   offset = 0;
#   magicOrExtension = "SQLite format 3\x00" + <skip 52 bytes> + "SELF";
#   mask = "\xff..\x00..\xff";  # match bytes 0-15 and 68-71
#   interpreter = "${self-exec}/bin/self-exec";
# };
```
