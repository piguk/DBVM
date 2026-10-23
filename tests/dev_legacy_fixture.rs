// Dev fixture (ignored by default; run with LEGACY_DB=<db> cargo test --test dev_legacy_fixture -- --ignored): collapse every multi-chunk file in $LEGACY_DB into one
// whole-file chunk, reproducing the pre-v5 layout so vm-rechunk can be measured.
#[test]
#[ignore]
fn make_legacy_layout() {
    let db = std::env::var("LEGACY_DB").expect("set LEGACY_DB");
    let c = selfdb::vm::vm_open(&db).unwrap();
    let paths: Vec<String> = c.prepare("SELECT path FROM vm_fs WHERE kind='file' AND size > 65536").unwrap()
        .query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    let mut n = 0;
    for p in paths {
        let whole = selfdb::vm::file_bytes(&c, &p).unwrap();
        let key = selfdb::vm::blob_key(&whole);
        let z = zstd::bulk::compress(&whole, 3).unwrap();
        c.execute("DELETE FROM vm_file_chunks WHERE path=?1", rusqlite::params![p]).unwrap();
        c.execute("INSERT OR IGNORE INTO vm_blobs(hash,content,compressed,raw_size,refcnt) VALUES (?1,?2,2,?3,0)",
            rusqlite::params![key.as_slice(), z, whole.len() as i64]).unwrap();
        c.execute("INSERT INTO vm_file_chunks(path,idx,hash) VALUES (?1,0,?2)", rusqlite::params![p, key.as_slice()]).unwrap();
        n += 1;
    }
    println!("legacy layout built for {} files", n);
}
