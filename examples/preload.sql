-- LD_PRELOAD 在 SELF 中是一张表，而不是环境变量
CREATE TABLE IF NOT EXISTS preload (ord INTEGER PRIMARY KEY, path TEXT);
INSERT INTO preload VALUES (0, 'libmul.so.1.self');
-- 再运行同一 ./app.self 即可看到不同的行为；回滚即 DELETE
-- DELETE FROM preload;
