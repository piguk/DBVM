use anyhow::{anyhow, Result};
use goblin::elf::{Elf, program_header::PT_NOTE};
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

pub const APP_ID: u32 = 0x53454C46;
pub const USER_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct Segment {
    pub typ: String,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub r: i32,
    pub w: i32,
    pub x: i32,
    pub align: u64,
    pub content: Option<Vec<u8>>,
}
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub version: Option<String>,
    pub value: u64,
    pub size: u64,
    pub typ: String,
    pub bind: String,
    pub defined: i32,
    pub exported: i32,
}
#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub typ: u32,
    pub offset: u64,
    pub size: u64,
    pub flags: u64,
}
#[derive(Debug, Clone)]
pub struct Note {
    pub typ: String,
    pub name: String,
    pub desc: Option<Vec<u8>>,
}
#[derive(Debug, Clone)]
pub struct DynamicEntry {
    pub tag: i64,
    pub value: u64,
}
fn seg_type(pt: u32) -> String {
    match pt {
        1 => "load".to_string(),
        7 => "tls".to_string(),
        0x6474e551 => "stack".to_string(),
        0x6474e552 => "relro".to_string(),
        4 => "note".to_string(),
        2 => "dynamic".to_string(),
        3 => "interp".to_string(),
        _ => format!("phdr[{}]", pt),
    }
}
#[derive(Debug, Clone)]
pub struct ElfInfo {
    pub data: Vec<u8>,
    pub is64: bool,
    pub endian: String,
    pub e_type: u16,
    pub e_machine: u16,
    pub e_entry: u64,
    pub e_phnum: usize,
    pub e_phentsize: u16,
    pub e_shnum: usize,
    pub e_shstrndx: usize,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub interp: Option<String>,
    pub segments: Vec<Segment>,
    pub symbols: Vec<Symbol>,
    pub sections: Vec<Section>,
    pub notes: Vec<Note>,
    pub dynamic_entries: Vec<DynamicEntry>,
    pub needed: Vec<String>,
    pub relocs: Vec<(u64,u64)>,
}
#[derive(Debug, Clone, Default)]
pub struct ElfMeta {
    pub needed: Vec<String>,
    pub soname: Option<String>,
    pub runpath: Option<String>,
    pub rpath: Option<String>,
}

fn parse_elf_bytes_inner(data: &[u8], path: &str, no_sections: bool, no_notes: bool) -> Result<ElfInfo> {
    if data.len() < 4 || &data[0..4] != b"\x7fELF" { return Err(anyhow!("not ELF: {}", path)); }
    let is64 = data[4]==2;
    let e_type = u16::from_le_bytes([data[16], data[17]]);
    let e_machine = u16::from_le_bytes([data[18], data[19]]);
    let e_entry = if is64 { u64::from_le_bytes(data[24..32].try_into()?) } else { u32::from_le_bytes(data[24..28].try_into()?) as u64 };
    let e_phentsize = u16::from_le_bytes([data[54], data[55]]);
    let e_phnum = u16::from_le_bytes([data[56], data[57]]) as usize;
    let e_flags = u32::from_le_bytes([data[48], data[49], data[50], data[51]]);
    let e_ehsize = u16::from_le_bytes([data[52], data[53]]);
    let e_shnum = u16::from_le_bytes([data[60], data[61]]) as usize;
    let e_shstrndx = u16::from_le_bytes([data[62], data[63]]) as usize;
    let endian = if data[5]==1 { "little" } else { "big" }.to_string();
    let elf = Elf::parse(data)?;
    let mut segments = Vec::with_capacity(elf.program_headers.len());
    for ph in &elf.program_headers {
        let off = ph.p_offset as usize;
        let content = if ph.p_filesz>0 && off < data.len() {
            let end = ((ph.p_offset+ph.p_filesz) as usize).min(data.len());
            Some(data[off..end].to_vec())
        } else { None };
        segments.push(Segment{
            typ: seg_type(ph.p_type),
            offset: ph.p_offset,
            vaddr: ph.p_vaddr,
            filesz: ph.p_filesz,
            memsz: ph.p_memsz,
            r: if ph.is_read() {1} else {0},
            w: if ph.is_write() {1} else {0},
            x: if ph.is_executable() {1} else {0},
            align: ph.p_align,
            content,
        });
    }
    let interp = elf.interpreter.map(|s| s.to_string());
    let mut sections = Vec::new();
    if !no_sections {
        sections.reserve(elf.section_headers.len());
        for sh in &elf.section_headers {
            let name = elf.shdr_strtab.get_at(sh.sh_name).unwrap_or("").to_string();
            sections.push(Section{ name, typ: sh.sh_type, offset: sh.sh_offset, size: sh.sh_size, flags: sh.sh_flags });
        }
    }
    let mut notes = Vec::new();
    if !no_notes {
        for ph in &elf.program_headers {
            if ph.p_type != PT_NOTE { continue; }
            let mut cur = ph.p_offset as usize;
            let end = (ph.p_offset + ph.p_filesz) as usize;
            while cur + 12 <= end && cur + 12 <= data.len() {
                let n_namesz = u32::from_le_bytes(data[cur..cur+4].try_into().unwrap_or([0;4])) as usize;
                let n_descsz = u32::from_le_bytes(data[cur+4..cur+8].try_into().unwrap_or([0;4])) as usize;
                let n_type = u32::from_le_bytes(data[cur+8..cur+12].try_into().unwrap_or([0;4]));
                cur += 12;
                let name = if n_namesz>0 && cur+n_namesz <= data.len() {
                    let s = &data[cur..cur+n_namesz];
                    let z = s.iter().position(|&b| b==0).unwrap_or(s.len());
                    String::from_utf8_lossy(&s[..z]).to_string()
                } else { "".to_string() };
                cur = (cur + n_namesz + 3) & !3;
                let desc = if n_descsz>0 && cur+n_descsz <= data.len() { Some(data[cur..cur+n_descsz].to_vec()) } else { None };
                cur = (cur + n_descsz + 3) & !3;
                notes.push(Note{ typ: n_type.to_string(), name, desc });
            }
        }
    }
    let mut dynamic_entries = Vec::new();
    if let Some(dynamic) = &elf.dynamic {
        dynamic_entries.reserve(dynamic.dyns.len());
        for d in &dynamic.dyns {
            dynamic_entries.push(DynamicEntry{ tag: d.d_tag as i64, value: d.d_val });
        }
    }
    let needed: Vec<String> = elf.libraries.iter().map(|s| s.to_string()).collect();
    let mut symbols = Vec::new();
    let syms_iter = if !elf.dynsyms.is_empty() { &elf.dynsyms } else { &elf.syms };
    let strtab = if !elf.dynsyms.is_empty() { &elf.dynstrtab } else { &elf.strtab };
    symbols.reserve(syms_iter.len());
    for sym in syms_iter.iter() {
        let name = strtab.get_at(sym.st_name).unwrap_or("").to_string();
        if name.is_empty() { continue; }
        let bind = sym.st_bind();
        let typ = sym.st_type();
        let defined = if sym.st_shndx != 0 {1} else {0};
        let exported = if defined==1 && (bind==1 || bind==2) {1} else {0};
        let typ_s = match typ { 2=>"func",1=>"object",6=>"tls",_=>"other"}.to_string();
        let bind_s = match bind {1=>"global",2=>"weak",0=>"local",_=>"other"}.to_string();
        symbols.push(Symbol{ name, version: None, value: sym.st_value, size: sym.st_size, typ: typ_s, bind: bind_s, defined, exported });
    }
    let mut relocs = Vec::new();
    for sh in &elf.shdr_relocs {
        for r in sh.1.iter() {
            if r.r_type == 37 {
                relocs.push((r.r_offset, r.r_addend.unwrap_or(0) as u64));
            }
        }
    }
    for r in elf.dynrelas.iter().chain(elf.pltrelocs.iter()) {
        if r.r_type == 37 {
            relocs.push((r.r_offset, r.r_addend.unwrap_or(0) as u64));
        }
    }
    relocs.sort_unstable();
    relocs.dedup();
    Ok(ElfInfo{
        data: data.to_vec(),
        is64, endian, e_type, e_machine, e_entry, e_phnum, e_phentsize, e_shnum, e_shstrndx, e_flags, e_ehsize,
        interp, segments, symbols, sections, notes, dynamic_entries, needed, relocs,
    })
}

pub fn parse_elf(path: &str, no_sections: bool, no_notes: bool) -> Result<ElfInfo> {
    let file = match File::open(path) { Ok(f)=>f, Err(e)=> return Err(anyhow!("open {}: {}", path, e)) };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 { return Err(anyhow!("empty: {}", path)); }
    if len > 64*1024*1024 {
        let data = std::fs::read(path)?;
        return parse_elf_bytes_inner(&data, path, no_sections, no_notes);
    }
    let mmap = unsafe { Mmap::map(&file).map_err(|e| anyhow!("mmap {}: {}", path, e))? };
    if mmap.len() < 4 { return Err(anyhow!("not ELF: {}", path)); }
    parse_elf_bytes_inner(&mmap, path, no_sections, no_notes)
}

pub fn meta_for_bytes(data: &[u8]) -> ElfMeta {
    let elf = match Elf::parse(data) { Ok(e)=>e, Err(_)=> return ElfMeta::default() };
    ElfMeta{
        needed: elf.libraries.iter().map(|s| s.to_string()).collect(),
        soname: elf.soname.map(|s| s.to_string()),
        runpath: elf.runpaths.get(0).map(|s| s.to_string()),
        rpath: elf.rpaths.get(0).map(|s| s.to_string()),
    }
}

pub fn meta_for_path(path: &str) -> ElfMeta {
    let file = match File::open(path) { Ok(f)=>f, Err(_)=> return ElfMeta::default() };
    let mmap = match unsafe { Mmap::map(&file) } { Ok(m)=>m, Err(_)=> return ElfMeta::default() };
    if mmap.is_empty() { return ElfMeta::default(); }
    meta_for_bytes(&mmap)
}

pub fn meta_for_path_cached(path: &Path, cache: &mut FxHashMap<PathBuf, ElfMeta>) -> ElfMeta {
    let key = path.to_path_buf();
    if let Some(m) = cache.get(&key) { return m.clone(); }
    let m = meta_for_path(&key.to_string_lossy());
    cache.insert(key.clone(), m.clone());
    m
}

pub fn needed_for_path(path: &str) -> Vec<String> { meta_for_path(path).needed }
pub fn soname_for_path(path: &str) -> Option<String> { meta_for_path(path).soname }
pub fn runpath_for_path(path: &str) -> (Option<String>, Option<String>) {
    let m = meta_for_path(path);
    (m.runpath, m.rpath)
}

pub fn needed_for_path_cached(path: &Path, cache: &mut FxHashMap<PathBuf, ElfMeta>) -> Vec<String> {
    meta_for_path_cached(path, cache).needed
}
pub fn soname_for_path_cached(path: &Path, cache: &mut FxHashMap<PathBuf, ElfMeta>) -> Option<String> {
    meta_for_path_cached(path, cache).soname
}
pub fn runpath_for_path_cached(path: &Path, cache: &mut FxHashMap<PathBuf, ElfMeta>) -> (Option<String>, Option<String>) {
    let m = meta_for_path_cached(path, cache);
    (m.runpath, m.rpath)
}
