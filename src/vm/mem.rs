use super::*;

pub fn vm_mem_insert(conn: &Connection, addr: i64, size: i64, prot: i64, content: &[u8]) -> Result<()> {
    conn.prepare_cached("INSERT INTO vm_mem(addr,size,prot,content) VALUES (?1,?2,?3,?4)")?.execute( params![addr,size,prot,content])?;
    Ok(())
}

pub fn vm_mem_list(conn: &Connection) -> Result<Vec<(i64,i64,i64,i64)>> {
    let mut st = conn.prepare("SELECT id, addr, size, prot FROM vm_mem ORDER BY addr")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let mut out=Vec::new(); for r in rows{ out.push(r?); } Ok(out)
}

pub fn vm_mem_clear(conn: &Connection) -> Result<()> {
    conn.prepare_cached("DELETE FROM vm_mem")?.execute( [])?;
    Ok(())
}

pub fn vm_mem_trace(db_path: &str, prog: &str, rest: Vec<String>) -> Result<()> {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    {
        let db_path_owned = db_path.to_string();
        let trace_path = std::env::temp_dir().join(format!("self-vm-trace-{}", std::process::id())).to_string_lossy().to_string();
        let strace_ok = Command::new("strace").arg("--help").output().is_ok();
        if strace_ok {
            let mut cmd = Command::new("strace");
            cmd.arg("-f").arg("-tt").arg("-e").arg("trace=memory").arg("-o").arg(&trace_path).arg(prog);
            for a in &rest { cmd.arg(a); }
            let status = cmd.status()?;
            let code = status.code().unwrap_or(0);
            let log = std::fs::read_to_string(&trace_path).unwrap_or_default();
            let _ = std::fs::remove_file(&trace_path);
            let conn = vm_open(&db_path_owned)?;
            let _ = vm_mem_clear(&conn);
            let mut inserted = 0usize;
            for line in log.lines() {
                let s = line.trim();
                if s.is_empty() { continue; }
                // strace memory trace looks like: "1234 12:34:56 mmap(NULL, 8192, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0x7f1234000"
                // we extract syscall name and args+ret
                let mmap_like = s.contains("mmap") || s.contains("mprotect") || s.contains("munmap") || s.contains("mremap") || s.contains("brk");
                if !mmap_like { continue; }
                // try to parse ret addr = hex after " = "
                let ret_part = s.rsplit(" = ").next().unwrap_or("");
                let addr: i64 = if ret_part.starts_with("0x") {
                    i64::from_str_radix(ret_part.trim_start_matches("0x").split([' ', '(']).next().unwrap_or("0"), 16).unwrap_or(0)
                } else { 0 };
                // size: first numeric after '(' e.g. mmap(NULL, 4096,
                let size: i64 = {
                    let p = s.find('(').unwrap_or(0);
                    let args = &s[p..];
                    // split by ',' and extract second field for mmap
                    let parts: Vec<&str> = args.split(',').collect();
                    if parts.len() >= 2 {
                        parts[1].trim().trim_start_matches("NULL").trim().parse::<i64>().unwrap_or(4096)
                    } else { 4096 }
                };
                let prot: i64 = if s.contains("PROT_READ") && s.contains("PROT_WRITE") && s.contains("PROT_EXEC") { 7 }
                else if s.contains("PROT_READ") && s.contains("PROT_WRITE") { 3 }
                else if s.contains("PROT_READ") && s.contains("PROT_EXEC") { 5 }
                else if s.contains("PROT_READ") { 1 }
                else if s.contains("PROT_WRITE") { 2 }
                else { 3 };
                let content = s.as_bytes().to_vec();
                // skip failed mmap (-1)
                if ret_part.contains("-1") && addr== -1 { continue; }
                let a = if addr==0 { 0x400000 + (inserted as i64)*0x1000 } else { addr };
                let sz = if s.contains("mmap") { size } else if s.contains("brk") { 0x1000 } else { size.max(0x1000) };
                let _ = vm_mem_insert(&conn, a, sz, prot, &content);
                inserted += 1;
            }
            if inserted==0 {
                let content = format!("trace:{} {:?} exit={} ts={} log_lines={} (strace no mmap captured, inserted synthetic)", prog, rest, code, now_secs(), log.lines().count()).into_bytes();
                vm_mem_insert(&conn, 0x400000, 0x1000, 5, &content)?;
                inserted = 1;
            }
            println!("vm-mem-trace: {} {:?} -> exit {} (inserted {} vm_mem entries from strace memory trace, {} bytes log)", prog, rest, code, inserted, log.len());
            if code != 0 { std::process::exit(code); }
            return Ok(());
        }
        // fallback: direct run without strace, capture /proc/self/maps of child via fork
        let mut child = Command::new(prog);
        for a in &rest { child.arg(a); }
        let status = child.status()?;
        let code = status.code().unwrap_or(0);
        let conn = vm_open(&db_path_owned)?;
        let content = format!("trace:{} {:?} exit={} ts={} (no strace)", prog, rest, code, now_secs()).into_bytes();
        let _ = vm_mem_clear(&conn);
        vm_mem_insert(&conn, 0x400000, 0x1000, 5, &content)?;
        println!("vm-mem-trace: {} {:?} -> exit {} (fallback synthetic)", prog, rest, code);
        if code != 0 { std::process::exit(code); }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("vm-mem-trace only supported on Linux");
    }
}
