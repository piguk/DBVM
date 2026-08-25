-- fully-SQL dynamic linker 思路（原文 §Dynamic linking 的 self-ld 路径示意）
-- 本 demo 的实际重定位表为 RELATIVE_RELOCS(vaddr, addend)，记录 R_X86_64_IRELATIVE；
-- self-exec 会在 mmap 后以 load_bias 修正并调用 addend 指向的 resolver。
-- 原文的抽象查询形如：
--   SELECT s.value + o.load_bias FROM relocations r JOIN symbols s ... WHERE r.id = ?
-- 在本 demo 中可近似为：
SELECT vaddr, addend FROM RELATIVE_RELOCS LIMIT 5;
SELECT name, version, value FROM symbols WHERE exported=1 LIMIT 5;
-- audit 方案则保留 ld.so，仅用 rtld-audit (la_objsearch) 把 RUNPATH 查找替换为 SQL 查询
