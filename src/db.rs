use rusqlite::{Connection, params};
use crate::elf::ElfInfo;

pub fn create_self_db(path: &str, info: &ElfInfo, no_sections: bool, no_notes: bool) -> anyhow::Result<()> {
    if std::path::Path::new(path).exists() { std::fs::remove_file(path)?; }
    let mut conn = Connection::open(path)?;
    conn.execute_batch(&format!("PRAGMA application_id = {}; PRAGMA user_version = 1;", crate::elf::APP_ID))?;
    conn.execute_batch(r#"
    CREATE TABLE self_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE self_blob(content BLOB NOT NULL);
    CREATE TABLE segments (id INTEGER PRIMARY KEY, type TEXT NOT NULL, offset INTEGER NOT NULL, vaddr INTEGER NOT NULL, filesz INTEGER NOT NULL, memsz INTEGER NOT NULL, r INTEGER, w INTEGER, x INTEGER, align INTEGER NOT NULL DEFAULT 4096, content BLOB);
    CREATE TABLE symbols (id INTEGER PRIMARY KEY, name TEXT NOT NULL, version TEXT, value INTEGER, size INTEGER, type TEXT, bind TEXT, defined INTEGER NOT NULL, exported INTEGER NOT NULL);
    CREATE INDEX idx_symbols_name ON symbols(name, version);
    CREATE TABLE sections (name TEXT, type INTEGER, offset INTEGER, size INTEGER, flags INTEGER);
    CREATE TABLE notes (type TEXT, name TEXT, desc BLOB);
    CREATE TABLE dynamic_entries (tag INTEGER NOT NULL, value INTEGER NOT NULL);
    CREATE TABLE needed (ord INTEGER PRIMARY KEY, soname TEXT NOT NULL);
    CREATE TABLE RELATIVE_RELOCS (vaddr INTEGER NOT NULL, addend INTEGER NOT NULL);
    CREATE VIEW exports AS SELECT name, version, type, size FROM symbols WHERE exported = 1;
    CREATE VIEW imports AS SELECT name, version FROM symbols WHERE defined = 0;
    CREATE VIEW ldd AS SELECT ord, soname FROM needed ORDER BY ord;
    "#)?;
    let tx = conn.transaction()?;
    {
        let mut stmt_meta = tx.prepare_cached("INSERT INTO self_meta VALUES (?1,?2)")?;
        let metas: Vec<(String,String)> = {
            let mut v = vec![
                ("class".to_string(), if info.is64 {"ELF64"} else {"ELF32"}.to_string()),
                ("endian".to_string(), info.endian.clone()),
                ("e_type".to_string(), info.e_type.to_string()),
                ("e_machine".to_string(), info.e_machine.to_string()),
                ("e_entry".to_string(), format!("{:#x}", info.e_entry)),
                ("e_phnum".to_string(), info.e_phnum.to_string()),
                ("e_phentsize".to_string(), info.e_phentsize.to_string()),
                ("e_shnum".to_string(), info.e_shnum.to_string()),
                ("e_shstrndx".to_string(), info.e_shstrndx.to_string()),
            ];
            if let Some(interp)=&info.interp { v.push(("interp".to_string(), interp.clone())); }
            v
        };
        for (k,v) in metas { stmt_meta.execute(params![k,v])?; }
        {
            let mut stmt = tx.prepare_cached("INSERT INTO segments VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?;
            for (i, seg) in info.segments.iter().enumerate() {
                let content: Option<&[u8]> = None;
                let _ = seg.content.as_ref();
                stmt.execute(params![i as i64, seg.typ, seg.offset as i64, seg.vaddr as i64, seg.filesz as i64, seg.memsz as i64, seg.r, seg.w, seg.x, seg.align as i64, content])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached("INSERT INTO symbols VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
            for (i, sym) in info.symbols.iter().enumerate() {
                stmt.execute(params![i as i64, sym.name, sym.version, sym.value as i64, sym.size as i64, sym.typ, sym.bind, sym.defined, sym.exported])?;
            }
        }
        if !no_sections && !info.sections.is_empty() {
            let mut stmt = tx.prepare_cached("INSERT INTO sections VALUES (?1,?2,?3,?4,?5)")?;
            for sec in &info.sections { stmt.execute(params![sec.name, sec.typ as i64, sec.offset as i64, sec.size as i64, sec.flags as i64])?; }
        }
        if !no_notes && !info.notes.is_empty() {
            let mut stmt = tx.prepare_cached("INSERT INTO notes VALUES (?1,?2,?3)")?;
            for note in &info.notes { stmt.execute(params![note.typ, note.name, note.desc.clone()])?; }
        }
        if !info.dynamic_entries.is_empty() {
            let mut stmt = tx.prepare_cached("INSERT INTO dynamic_entries VALUES (?1,?2)")?;
            for d in &info.dynamic_entries { stmt.execute(params![d.tag, d.value as i64])?; }
        }
        if !info.needed.is_empty() {
            let mut stmt = tx.prepare_cached("INSERT INTO needed VALUES (?1,?2)")?;
            for (i, soname) in info.needed.iter().enumerate() { stmt.execute(params![i as i64, soname])?; }
        }
        if !info.relocs.is_empty() {
            let mut stmt = tx.prepare_cached("INSERT INTO RELATIVE_RELOCS VALUES (?1,?2)")?;
            for (vaddr, addend) in &info.relocs { stmt.execute(params![*vaddr as i64, *addend as i64])?; }
        }
        {
            let mut stmt = tx.prepare_cached("INSERT INTO self_blob VALUES (?1)")?;
            stmt.execute(params![info.data])?;
        }
    }
    tx.commit()?;
    Ok(())
}
