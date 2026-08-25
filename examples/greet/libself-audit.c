// rtld-audit demo stub (原文 §Dynamic linking 路径 a)
// 保留 ld.so，仅用 la_objsearch 把 RUNPATH 寻库替换为 SQL 查询
// 演示：SELF_SYSTEM_DB 指向 closure.db 时，解析 soname -> resolved_path
// 编译: gcc -fPIC -shared -o libself-audit.so libself-audit.c -lsqlite3
#include <link.h>
#include <stdlib.h>
#include <string.h>
#include <sqlite3.h>

unsigned int la_version(unsigned int v){ return v; }

char *la_objsearch(const char *name, uintptr_t *cookie, unsigned int flag){
    const char *dbpath = getenv("SELF_SYSTEM_DB");
    if(!dbpath || !name) return (char*)name;
    sqlite3 *db = NULL;
    if(sqlite3_open(dbpath, &db) != SQLITE_OK) return (char*)name;
    // closure.db : needs(soname, resolved_path) ; direct lookup
    sqlite3_stmt *st = NULL;
    // Basename match: soname is basename
    const char *base = strrchr(name, '/');
    const char *soname = base ? base+1 : name;
    static char resolved[1024];
    resolved[0] = '\0';
    if(sqlite3_prepare_v2(db, "SELECT resolved_path FROM needs WHERE soname=? AND resolved_path IS NOT NULL LIMIT 1", -1, &st, NULL)==SQLITE_OK){
        sqlite3_bind_text(st, 1, soname, -1, SQLITE_STATIC);
        if(sqlite3_step(st)==SQLITE_ROW){
            const unsigned char *p = sqlite3_column_text(st, 0);
            if(p) { strncpy(resolved, (const char*)p, sizeof(resolved)-1); }
        }
        sqlite3_finalize(st);
    }
    sqlite3_close(db);
    if(resolved[0]) return resolved;
    return (char*)name;
}
unsigned int la_objopen(struct link_map *map, long lmid, uintptr_t *cookie){ return 0; }
