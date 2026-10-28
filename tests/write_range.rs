//! write_range rewrites only the chunks a write touches. Every result must equal a plain in-memory
//! model of the same file, byte for byte, and the database must still verify deep.
use selfdb::vm::{self, CHUNK_SIZE};

fn make(name: &str) -> (std::path::PathBuf, rusqlite::Connection) {
    let d = std::env::temp_dir().join(format!("wr-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("v.db");
    let c = vm::init_vm_db_with_opts(db.to_str().unwrap(), true, true).unwrap();
    (d, c)
}

fn check(c: &rusqlite::Connection, path: &str, model: &[u8]) {
    let got = vm::read_range(c, path, 0, usize::MAX).unwrap();
    assert_eq!(got.len(), model.len(), "length differs from model");
    assert!(got == model, "content differs from model");
    let size: i64 = c.query_row("SELECT size FROM vm_fs WHERE path=?1", [path], |r| r.get(0)).unwrap();
    assert_eq!(size as usize, model.len());
}

#[test]
fn writes_inside_across_and_past_the_end_match_a_model() {
    let (d, c) = make("edges");
    let mut model: Vec<u8> = (0..(3 * CHUNK_SIZE + 100)).map(|i| (i % 251) as u8).collect();
    vm::vm_add_bytes(&c, "/f", &model, 0o644, 1).unwrap();
    check(&c, "/f", &model);
    // inside one chunk
    let w = vec![0xAA; 10];
    vm::write_range(&c, "/f", 5, &w, 2).unwrap();
    model[5..15].copy_from_slice(&w);
    check(&c, "/f", &model);
    // across a chunk boundary
    let w = vec![0xBB; 2000];
    let off = CHUNK_SIZE - 1000;
    vm::write_range(&c, "/f", off as u64, &w, 3).unwrap();
    model[off..off + 2000].copy_from_slice(&w);
    check(&c, "/f", &model);
    // extending the end
    let w = vec![0xCC; 500];
    let off = model.len();
    vm::write_range(&c, "/f", off as u64, &w, 4).unwrap();
    model.extend_from_slice(&w);
    check(&c, "/f", &model);
    // past the end with a gap: the gap must read as zeros
    let w = vec![0xDD; 7];
    let off = model.len() + 2 * CHUNK_SIZE + 9;
    vm::write_range(&c, "/f", off as u64, &w, 5).unwrap();
    model.resize(off, 0);
    model.extend_from_slice(&w);
    check(&c, "/f", &model);
    // whole-file hash must match the content
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "deep verify: {}", rep);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn random_writes_match_a_model_and_stay_consistent() {
    let (d, c) = make("random");
    let mut model: Vec<u8> = vec![7u8; 5 * CHUNK_SIZE];
    vm::vm_add_bytes(&c, "/r", &model, 0o644, 1).unwrap();
    let mut rng: u64 = 0x9E3779B97F4A7C15;
    let mut next = |m: u64| { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng % m };
    for round in 0..120u64 {
        let len = 1 + next(3 * CHUNK_SIZE as u64) as usize;
        let off = next((model.len() + CHUNK_SIZE) as u64) as usize;
        let data: Vec<u8> = (0..len).map(|i| (round as u8).wrapping_add(i as u8)).collect();
        vm::write_range(&c, "/r", off as u64, &data, round as i64).unwrap();
        if off > model.len() { model.resize(off, 0); }
        if off + len > model.len() { model.resize(off + len, 0); }
        model[off..off + len].copy_from_slice(&data);
        if round % 10 == 9 { check(&c, "/r", &model); }
    }
    check(&c, "/r", &model);
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "deep verify: {}", rep);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_write_rewrites_only_the_touched_chunks() {
    let (d, c) = make("touch");
    let model: Vec<u8> = (0..(8 * CHUNK_SIZE)).map(|i| (i / CHUNK_SIZE) as u8).collect();
    vm::vm_add_bytes(&c, "/t", &model, 0o644, 1).unwrap();
    let before: Vec<(i64, Vec<u8>)> = c.prepare("SELECT idx, hash FROM vm_file_chunks WHERE path='/t' ORDER BY idx").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
    // one write inside chunk 3 only
    vm::write_range(&c, "/t", (3 * CHUNK_SIZE + 10) as u64, &[0xEE; 20], 2).unwrap();
    let after: Vec<(i64, Vec<u8>)> = c.prepare("SELECT idx, hash FROM vm_file_chunks WHERE path='/t' ORDER BY idx").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
    let changed: Vec<i64> = before.iter().zip(after.iter()).filter(|(a, b)| a != b).map(|(a, _)| a.0).collect();
    assert_eq!(changed, vec![3], "only chunk 3 may change");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn block_write_and_full_write_agree_on_content_and_whole_hash() {
    // The same final content, produced two ways, must give the same whole-file hash. Content-addressed
    // blob keys make the chunks identical; vm_fs.hash must match too (it feeds sync).
    let (d, c) = make("agree");
    let mut model: Vec<u8> = (0..(2 * CHUNK_SIZE + 500)).map(|i| (i % 97) as u8).collect();
    vm::vm_add_bytes(&c, "/a", &model, 0o644, 1).unwrap();
    vm::vm_add_bytes(&c, "/b", &model, 0o644, 1).unwrap();
    let patch = vec![0x5A; 3000];
    vm::write_range(&c, "/a", (CHUNK_SIZE - 40) as u64, &patch, 2).unwrap();
    model[CHUNK_SIZE - 40..CHUNK_SIZE - 40 + 3000].copy_from_slice(&patch);
    vm::vm_add_bytes(&c, "/b", &model, 0o644, 2).unwrap();
    let ha: Vec<u8> = c.query_row("SELECT hash FROM vm_fs WHERE path='/a'", [], |r| r.get(0)).unwrap();
    let hb: Vec<u8> = c.query_row("SELECT hash FROM vm_fs WHERE path='/b'", [], |r| r.get(0)).unwrap();
    assert_eq!(ha, hb, "block write and full write must produce the same whole-file hash");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn truncate_shrinks_and_grows_like_a_model_and_stays_consistent() {
    let (d, c) = make("trunc");
    let mut model: Vec<u8> = (0..(4 * CHUNK_SIZE + 300)).map(|i| (i % 233) as u8).collect();
    vm::vm_add_bytes(&c, "/t", &model, 0o644, 1).unwrap();
    // shrink to a chunk boundary, mid-chunk, to zero, then grow past several chunks
    for (round, new_len) in [2 * CHUNK_SIZE, 2 * CHUNK_SIZE + 17, 5, 0, 3 * CHUNK_SIZE + 9].into_iter().enumerate() {
        vm::truncate_range(&c, "/t", new_len as u64, round as i64).unwrap();
        model.resize(new_len, 0);
        check(&c, "/t", &model);
        let rep = vm::vm_verify(&c, true).unwrap();
        assert!(rep.contains("deep=ok"), "after truncate to {}: {}", new_len, rep);
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn shrinking_then_writing_does_not_resurrect_old_bytes() {
    // Truncate mid-chunk, then grow again: the bytes between the old and new end must read as zero,
    // not as the stale tail of the earlier content.
    let (d, c) = make("noresurrect");
    vm::vm_add_bytes(&c, "/n", &vec![0xFFu8; CHUNK_SIZE + 500], 0o644, 1).unwrap();
    vm::truncate_range(&c, "/n", 100, 2).unwrap();
    vm::truncate_range(&c, "/n", (CHUNK_SIZE + 500) as u64, 3).unwrap();
    let got = vm::read_range(&c, "/n", 0, usize::MAX).unwrap();
    assert!(got[..100].iter().all(|&b| b == 0xFF), "kept prefix");
    assert!(got[100..].iter().all(|&b| b == 0), "regrown tail must be zero, not stale 0xFF");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn truncate_leaves_no_bytes_past_the_new_end_in_storage() {
    // The stored last chunk must decode to exactly the new size. Reads clamp to the file size, so a
    // read cannot show stale bytes sitting past the end in the blob; decode the stored block directly.
    let (d, c) = make("nostale");
    vm::vm_add_bytes(&c, "/s", &vec![0xFFu8; CHUNK_SIZE + 500], 0o644, 1).unwrap();
    vm::truncate_range(&c, "/s", 100, 2).unwrap();
    let (body, comp, dict_id): (Vec<u8>, i64, i64) = c.query_row(
        "SELECT b.content, b.compressed, b.dict_id FROM vm_file_chunks c JOIN vm_blobs b ON b.hash=c.hash
         WHERE c.path='/s' AND c.idx=0", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    let stored = vm::decode_block(&c, &body, comp, dict_id).unwrap();
    assert_eq!(stored.len(), 100, "stored last chunk must end at the new size, not keep old bytes");
    assert!(stored.iter().all(|&b| b == 0xFF), "the kept prefix is preserved");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rewriting_a_chunk_with_identical_bytes_keeps_its_blob() {
    // Regression: a write whose bytes equal a chunk's current bytes produces the same hash. The old
    // code deleted the row first. If that row was the blob's only reference (refcount 1), vm_fc_del
    // dropped the blob, and the re-insert then pointed at nothing. Each chunk here has unique content,
    // so every blob is referenced exactly once.
    let (d, c) = make("identical");
    let data: Vec<u8> = (0..(3 * CHUNK_SIZE)).map(|i| ((i / CHUNK_SIZE) as u8).wrapping_mul(37).wrapping_add((i % 251) as u8)).collect();
    vm::vm_add_bytes(&c, "/i", &data, 0o644, 1).unwrap();
    // rewrite the middle chunk's own bytes over themselves
    let mid = &data[CHUNK_SIZE + 4096..CHUNK_SIZE + 8192];
    vm::write_range(&c, "/i", (CHUNK_SIZE + 4096) as u64, mid, 2).unwrap();
    let got = vm::read_range(&c, "/i", 0, usize::MAX).unwrap();
    assert_eq!(got, data, "content unchanged");
    let rep = vm::vm_verify(&c, true).unwrap();
    assert!(rep.contains("deep=ok"), "deep verify: {}", rep);
    let _ = std::fs::remove_dir_all(&d);
}

fn sha256(b: &[u8]) -> Vec<u8> { use sha2::{Digest, Sha256}; Sha256::digest(b).to_vec() }

fn stored_hash(c: &rusqlite::Connection, path: &str) -> Vec<u8> {
    c.query_row("SELECT hash FROM vm_fs WHERE path=?1", [path], |r| r.get(0)).unwrap()
}

fn dirty(c: &rusqlite::Connection) -> i64 { c.query_row("SELECT count(*) FROM vm_dirty", [], |r| r.get(0)).unwrap() }

#[test]
fn a_deferred_write_is_seen_by_snapshots_and_diffs() {
    // A deferred write leaves the whole-file hash stale until something reads it. A snapshot diff that
    // trusted the stale hash would report the file as unchanged.
    let (d, c) = make("deferred-diff");
    let mut model = vec![7u8; 2 * CHUNK_SIZE + 9];
    vm::vm_add_bytes(&c, "/f", &model, 0o644, 1).unwrap();
    let (_, before) = vm::snapshot_create_named(&c, "s", "").unwrap();
    for (i, off) in [0usize, 4096, CHUNK_SIZE + 3, 2 * CHUNK_SIZE].into_iter().enumerate() {
        let w = vec![i as u8 + 1; 100];
        vm::write_range_deferred(&c, "/f", off as u64, &w, 2, None).unwrap();
        let end = (off + w.len()).max(model.len());
        model.resize(end, 0);
        model[off..off + w.len()].copy_from_slice(&w);
    }
    check(&c, "/f", &model);
    assert_eq!(dirty(&c), 1, "the file is recorded as pending");
    let diff = vm::snapshot_diff_live(&c, &before).unwrap();
    assert_eq!(diff.modified, vec!["/f".to_string()], "the diff sees the deferred write");
    assert_eq!(dirty(&c), 0, "the diff finalized it");
    assert_eq!(stored_hash(&c, "/f"), sha256(&model), "the finalized hash is the content's sha256");
    assert!(vm::vm_verify(&c, true).unwrap().contains("deep=ok"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_pending_hash_follows_a_rename_and_survives_a_reopen() {
    // vm_dirty keys on the path; the foreign key carries the row through a rename, and a writer that
    // stops before finalizing (a crash) leaves it for the next open.
    let (d, c) = make("deferred-rename");
    vm::vm_add_bytes(&c, "/a", b"old content", 0o644, 1).unwrap();
    vm::write_range_deferred(&c, "/a", 0, b"NEW", 2, None).unwrap();
    c.execute("UPDATE vm_fs SET path='/b' WHERE path='/a'", []).unwrap();
    c.execute("UPDATE vm_file_chunks SET path='/b' WHERE path='/a'", []).unwrap();
    let pending: String = c.query_row("SELECT path FROM vm_dirty", [], |r| r.get(0)).unwrap();
    assert_eq!(pending, "/b");
    drop(c);
    let c = vm::vm_open(d.join("v.db").to_str().unwrap()).unwrap();
    assert_eq!(dirty(&c), 0, "opening the database finalizes pending hashes");
    assert_eq!(stored_hash(&c, "/b"), sha256(b"NEW content"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_deferred_truncate_is_finalized_like_a_write() {
    let (d, c) = make("deferred-trunc");
    let data: Vec<u8> = (0..(2 * CHUNK_SIZE + 500)).map(|i| (i % 253) as u8).collect();
    vm::vm_add_bytes(&c, "/f", &data, 0o644, 1).unwrap();
    vm::truncate_range_deferred(&c, "/f", CHUNK_SIZE as u64 + 10, 2, None).unwrap();
    check(&c, "/f", &data[..CHUNK_SIZE + 10]);
    assert_eq!(vm::finalize_dirty(&c).unwrap(), 1);
    assert_eq!(stored_hash(&c, "/f"), sha256(&data[..CHUNK_SIZE + 10]));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_database_with_pending_hashes_is_refused_as_base_or_package() {
    // A Base or a package is attached read-only, so its stale hashes cannot be finalized, and copying
    // them would record wrong content hashes in the target.
    let (d, c) = make("deferred-ro");
    vm::vm_add_bytes(&c, "/f", b"abc", 0o644, 1).unwrap();
    vm::write_range_deferred(&c, "/f", 0, b"X", 2, None).unwrap();
    // closing a connection does not finalize: the database is left with a pending hash, as a crashed
    // writer would leave it
    drop(c);
    let db = d.join("v.db");
    let e = selfdb::overlay::Overlay::open(&d.join("delta.db"), &db).err().expect("base with pending hashes refused");
    assert!(e.to_string().contains("pending hashes"), "{}", e);
    let target = vm::init_vm_db_with_opts(d.join("t.db").to_str().unwrap(), true, true).unwrap();
    let e = selfdb::inject::inject(&target, &db).expect_err("package with pending hashes refused");
    assert!(e.to_string().contains("pending hashes"), "{}", e);
    let _ = std::fs::remove_dir_all(&d);
}
