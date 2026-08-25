-- patchelf 在 SELF 中是 UPDATE，而不是偏移手术
-- 改 soname / RUNPATH 等价于改 needed / dynamic_entries
UPDATE needed SET soname = 'libc.so.6' WHERE ord = 0;
-- 或改某段权限 / vaddr
UPDATE segments SET r = 1, w = 0, x = 1 WHERE type = 'load' AND id = 1;
