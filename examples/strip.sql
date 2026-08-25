-- strip(1) 在 SELF 中是一个事务：删除可选表后 VACUUM
-- 可选表：sections, notes, dynamic_entries；保留 self_meta / segments / symbols / needed 仍可运行
DELETE FROM sections;
DELETE FROM notes;
DELETE FROM dynamic_entries;
VACUUM;
-- 原文示例：57344 -> 49152 bytes，程序仍可 ./hello
