//! Tool injection merges a package DB into a target in one transaction. Regression for the plan's
//! SELECT * form: copying vm_blobs.id and vm_fs.id collided with the target's own rows and silently
//! dropped data, leaving a file whose chunks pointed at nothing.
use selfdb::{inject, vm};
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("inj-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn package(d: &Path, files: &[(&str, &[u8])]) -> PathBuf {
    let p = d.join("pkg.db");
    let c = vm::init_vm_db_with_opts(p.to_str().unwrap(), true, true).unwrap();
    for (path, data) in files { vm::vm_add_bytes(&c, path, data, 0o755, 1).unwrap(); }
    drop(c);
    p
}

fn target(d: &Path, files: &[(&str, &[u8])]) -> rusqlite::Connection {
    let c = vm::init_vm_db_with_opts(d.join("tgt.db").to_str().unwrap(), true, true).unwrap();
    for (path, data) in files { vm::vm_add_bytes(&c, path, data, 0o644, 1).unwrap(); }
    c
}

#[test]
fn injects_new_paths_and_replaces_existing_ones() {
    let d = tmp("replace");
    let pkg = package(&d, &[("/usr/bin/tool", b"TOOL-NEW"), ("/usr/lib/libx.so", b"LIB-BYTES")]);
    let tgt = target(&d, &[("/usr/bin/tool", b"TOOL-OLD"), ("/keep/a", b"keep-me")]);
    let rep = inject::inject(&tgt, &pkg).unwrap();
    // the two files plus the directories they need (/usr, /usr/bin, /usr/lib); the root is not counted
    assert_eq!(rep.paths, 5, "files and their parent directories");
    // replaced = the package paths the target already had: /usr, /usr/bin (directories) and /usr/bin/tool
    assert_eq!(rep.replaced, 3, "paths the target already had are replaced, not duplicated");
    assert_eq!(vm::read_range(&tgt, "/usr/bin/tool", 0, 100).unwrap(), b"TOOL-NEW");
    assert_eq!(vm::read_range(&tgt, "/usr/lib/libx.so", 0, 100).unwrap(), b"LIB-BYTES");
    assert_eq!(vm::read_range(&tgt, "/keep/a", 0, 100).unwrap(), b"keep-me", "unrelated path untouched");
    let v = vm::vm_verify(&tgt, true).unwrap();
    assert!(v.contains("deep=ok"), "target must verify deep: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_blob_already_in_the_target_is_shared_not_duplicated() {
    // The package's file has the same content as one the target already has: one blob, not two.
    let d = tmp("shared");
    let pkg = package(&d, &[("/usr/bin/same", b"SAME-CONTENT")]);
    let tgt = target(&d, &[("/other/x", b"SAME-CONTENT")]);
    let before: i64 = tgt.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    let rep = inject::inject(&tgt, &pkg).unwrap();
    let after: i64 = tgt.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(rep.blobs_added, 0, "identical content adds no blob");
    assert_eq!(before, after);
    assert_eq!(vm::read_range(&tgt, "/usr/bin/same", 0, 100).unwrap(), b"SAME-CONTENT");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_package_blob_is_never_dropped_because_of_an_id_collision() {
    // Regression: the target's blob ids and the package's blob ids overlap. Copying by name keeps every
    // package blob; the chunk that needs it must still read back.
    let d = tmp("idcollide");
    let pkg = package(&d, &[("/usr/bin/tool", b"TOOL-BYTES-XYZ")]);
    // many target blobs so that the target's ids cover the package's id range
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..5 { files.push((format!("/pad/{}", i), format!("pad-{}", i).into_bytes())); }
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    let tgt = target(&d, &refs);
    inject::inject(&tgt, &pkg).unwrap();
    assert_eq!(vm::read_range(&tgt, "/usr/bin/tool", 0, 100).unwrap(), b"TOOL-BYTES-XYZ");
    let v = vm::vm_verify(&tgt, true).unwrap();
    assert!(v.contains("deep=ok"), "no chunk may reference a missing blob: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_package_that_is_not_a_vm_database_leaves_the_target_untouched() {
    let d = tmp("notvm");
    let junk = d.join("junk.db");
    rusqlite::Connection::open(&junk).unwrap().execute_batch("CREATE TABLE x(y);").unwrap();
    let tgt = target(&d, &[("/keep/a", b"keep-me")]);
    let before = vm::read_range(&tgt, "/keep/a", 0, 100).unwrap();
    assert!(inject::inject(&tgt, &junk).is_err(), "a non-VM package is refused");
    assert_eq!(vm::read_range(&tgt, "/keep/a", 0, 100).unwrap(), before);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn inject_does_not_modify_the_package() {
    let d = tmp("pkgro");
    let pkg = package(&d, &[("/usr/bin/tool", b"TOOL")]);
    let before = std::fs::read(&pkg).unwrap();
    let tgt = target(&d, &[]);
    inject::inject(&tgt, &pkg).unwrap();
    assert_eq!(std::fs::read(&pkg).unwrap(), before, "the package file must not change");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_dictionary_compressed_package_file_reads_back_after_injection() {
    // Regression: the package's file is compressed with a dictionary the target does not have. Without
    // copying and remapping the dictionary, the injected file is unreadable ("missing dictionary").
    let d = tmp("dict");
    let pkg_path = d.join("pkg.db");
    let pkg = vm::init_vm_db_with_opts(pkg_path.to_str().unwrap(), true, true).unwrap();
    for i in 0..200 {
        let body = format!("common header line {}\n{}", i, "shared shared shared shared text ".repeat(20));
        vm::vm_add_bytes(&pkg, &format!("/lib/s{}", i), body.as_bytes(), 0o644, 1).unwrap();
    }
    assert!(vm::vm_train_dict(&pkg, 16384).unwrap().is_some(), "dictionary must train for this test");
    let big = format!("common header line 999\n{}", "shared shared shared shared text ".repeat(20));
    vm::vm_add_bytes(&pkg, "/usr/bin/dicted", big.as_bytes(), 0o755, 2).unwrap();
    let uses_dict: i64 = pkg.query_row(
        "SELECT count(*) FROM vm_file_chunks c JOIN vm_blobs b ON b.hash=c.hash WHERE c.path='/usr/bin/dicted' AND b.dict_id<>0",
        [], |r| r.get(0)).unwrap();
    assert!(uses_dict > 0, "the package file must really be dictionary-compressed");
    drop(pkg);
    // the target has its own dictionary (id 1) for different content: the ids must not be confused
    let tgt = vm::init_vm_db_with_opts(d.join("tgt.db").to_str().unwrap(), true, true).unwrap();
    for i in 0..200 {
        let body = format!("target-own text {} {}", i, "other other other ".repeat(30));
        vm::vm_add_bytes(&tgt, &format!("/own/t{}", i), body.as_bytes(), 0o644, 1).unwrap();
    }
    assert!(vm::vm_train_dict(&tgt, 16384).unwrap().is_some());
    let own_before = vm::read_range(&tgt, "/own/t5", 0, usize::MAX).unwrap();
    let rep = inject::inject(&tgt, &pkg_path).unwrap();
    assert!(rep.dicts_added >= 1, "the package dictionary is copied in");
    assert_eq!(vm::read_range(&tgt, "/usr/bin/dicted", 0, usize::MAX).unwrap(), big.as_bytes(),
        "the injected file must read back exactly");
    assert_eq!(vm::read_range(&tgt, "/own/t5", 0, usize::MAX).unwrap(), own_before,
        "the target's own dictionary-compressed files must still read the same");
    let v = vm::vm_verify(&tgt, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_blob_shared_with_a_replaced_target_file_survives_the_replacement() {
    // The target's /usr/bin/tool holds content X. The package's /usr/bin/tool also holds X, and a second
    // package file /usr/lib/copy holds X too. Replacing /usr/bin/tool releases the target's reference to X.
    // X must still be there for /usr/lib/copy, which is injected after it.
    let d = tmp("shared-replace");
    let x: &[u8] = b"SHARED-CONTENT-X-0123456789";
    let pkg = package(&d, &[("/usr/bin/tool", x), ("/usr/lib/copy", x)]);
    let tgt = target(&d, &[("/usr/bin/tool", b"OLD-DIFFERENT-CONTENT")]);
    inject::inject(&tgt, &pkg).unwrap();
    assert_eq!(vm::read_range(&tgt, "/usr/bin/tool", 0, 100).unwrap(), x);
    assert_eq!(vm::read_range(&tgt, "/usr/lib/copy", 0, 100).unwrap(), x, "the shared blob must survive");
    let v = vm::vm_verify(&tgt, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_package_with_many_files_injects_completely() {
    // Regression for the e2e failure: 35 of 40 blobs were lost when all blobs were copied up front.
    let d = tmp("many");
    let contents: Vec<(String, Vec<u8>)> = (0..40)
        .map(|i| (format!("/lib/f{}", i), format!("content number {} ", i).repeat(50 + i).into_bytes()))
        .collect();
    let refs: Vec<(&str, &[u8])> = contents.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    let pkg = package(&d, &refs);
    let tgt = target(&d, &[("/keep/a", b"keep")]);
    inject::inject(&tgt, &pkg).unwrap();
    for (p, b) in &contents {
        assert_eq!(vm::read_range(&tgt, p, 0, usize::MAX).unwrap(), *b, "{} must read back", p);
    }
    let v = vm::vm_verify(&tgt, true).unwrap();
    assert!(v.contains("deep=ok"), "deep verify: {}", v);
    let _ = std::fs::remove_dir_all(&d);
}
