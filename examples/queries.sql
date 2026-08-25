-- readelf / ldd / nm 的 SQL 等价（来自原文）
-- ldd
SELECT soname FROM ldd;
-- nm -D --undefined 的前 3 条
SELECT name, version FROM imports LIMIT 3;
-- exports
SELECT name, version, type, size FROM exports LIMIT 5;
-- readelf -l 的 load 段
SELECT type, vaddr, memsz, r, w, x FROM segments WHERE type='load';
-- section/header 视作可删除的工具表
SELECT name, type FROM sections LIMIT 5;
SELECT type, name FROM notes;
