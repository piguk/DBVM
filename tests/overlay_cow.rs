//! Block-level copy-on-write in the overlay: a small write to a large Base file moves only the chunks
//! it touches into Delta, and reads still return the whole file.
use selfdb::vm;
use selfdb::vm::CHUNK_SIZE;

fn make(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let d = std::env::temp_dir().join(format!("cow-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let base = d.join("base.db");
    let b = vm::init_vm_db_with_opts(base.to_str().unwrap(), true, true).unwrap();
    // a 64 MiB file whose every chunk has distinct content
    let data: Vec<u8> = (0..16 * 1024 * 1024).flat_map(|i: u32| i.to_le_bytes()).collect();
    assert_eq!(data.len(), 64 * 1024 * 1024);
    vm::vm_add_bytes(&b, "/big.bin", &data, 0o644, 1).unwrap();
    drop(b);
    let delta = d.join("delta.db");
    drop(vm::Overlay::open(&delta, &base).unwrap());
    (d, base, delta)
}

#[test]
fn a_small_write_to_a_base_file_copies_only_the_touched_chunks() {
    let (d, base, delta) = make("small");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let before = vm::read_range(&vm::vm_open(base.to_str().unwrap()).unwrap(), "/big.bin", 0, usize::MAX).unwrap();

    // one 4 KiB write in the middle of chunk 7
    let off = 7 * CHUNK_SIZE + 1234;
    let patch = vec![0xABu8; 4096];
    ov.write_range("/big.bin", off as u64, &patch, 2).unwrap();

    // Delta holds only the chunk the write touched; the rest is still served from Base
    let dc = rusqlite::Connection::open(&delta).unwrap();
    let rows: i64 = dc.query_row("SELECT count(*) FROM vm_file_chunks WHERE path='/big.bin'", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "one 4 KiB write copies exactly one chunk, got {}", rows);
    let idx: i64 = dc.query_row("SELECT idx FROM vm_file_chunks WHERE path='/big.bin'", [], |r| r.get(0)).unwrap();
    assert_eq!(idx, 7, "the touched chunk index");
    let blobs: i64 = dc.query_row("SELECT count(*) FROM vm_blobs", [], |r| r.get(0)).unwrap();
    assert_eq!(blobs, 1, "one blob, not the whole file: {}", blobs);

    // the whole file still reads correctly, byte for byte against the original plus the patch
    let got = ov.read("/big.bin").unwrap();
    assert_eq!(got.len(), before.len(), "size preserved");
    let mut want = before.clone();
    want[off..off + 4096].copy_from_slice(&patch);
    assert!(got == want, "content must equal the original with the patch applied");

    // the Base file itself was never written
    let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
    let base_now = vm::read_range(&bc, "/big.bin", 0, usize::MAX).unwrap();
    assert!(base_now == before, "Base content must be untouched");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_partial_read_returns_the_same_bytes_as_the_full_read() {
    let (d, base, delta) = make("range");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.write_range("/big.bin", (5 * CHUNK_SIZE) as u64, &[9u8; 100], 2).unwrap();
    let full = ov.read("/big.bin").unwrap();
    for (off, len) in [(0usize, 10usize), (CHUNK_SIZE - 5, 20), (5 * CHUNK_SIZE - 3, 10), (5 * CHUNK_SIZE, 50), (full.len() - 7, 100)] {
        let part = ov.read_range("/big.bin", off as u64, len).unwrap();
        let want = &full[off.min(full.len())..(off + len).min(full.len())];
        assert!(part == want, "range {}..{} must match the full read", off, off + len);
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn truncate_shrinks_and_grows_through_the_overlay() {
    let (d, base, delta) = make("trunc");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    let orig = ov.read("/big.bin").unwrap();
    // shrink to a chunk boundary plus a bit
    let new_len = 3 * CHUNK_SIZE + 77;
    ov.truncate_range("/big.bin", new_len as u64, 2).unwrap();
    let got = ov.read("/big.bin").unwrap();
    assert_eq!(got.len(), new_len, "shrunk size");
    assert!(got == orig[..new_len], "the kept prefix is unchanged");
    // grow again: the new tail reads as zeros, not as the old bytes
    ov.truncate_range("/big.bin", (4 * CHUNK_SIZE) as u64, 3).unwrap();
    let grown = ov.read("/big.bin").unwrap();
    assert_eq!(grown.len(), 4 * CHUNK_SIZE);
    assert!(grown[..new_len] == orig[..new_len], "kept prefix unchanged after growth");
    assert!(grown[new_len..].iter().all(|&b| b == 0), "regrown tail must be zero");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn deep_verify_passes_after_cow_writes() {
    let (d, base, delta) = make("verify");
    let ov = vm::Overlay::open(&delta, &base).unwrap();
    ov.write_range("/big.bin", 1000, &[7u8; 5000], 2).unwrap();
    ov.write_range("/big.bin", (9 * CHUNK_SIZE) as u64, &[8u8; 70000], 3).unwrap();
    ov.truncate_range("/big.bin", (2 * CHUNK_SIZE) as u64, 4).unwrap();
    let dc = rusqlite::Connection::open(&delta).unwrap();
    let v = vm::vm_verify(&dc, true).unwrap();
    assert!(v.contains("deep=ok"), "Delta must verify deep: {}", v);
    let bc = vm::vm_open(base.to_str().unwrap()).unwrap();
    let bv = vm::vm_verify(&bc, true).unwrap();
    assert!(bv.contains("deep=ok"), "Base must still verify deep: {}", bv);
    let _ = std::fs::remove_dir_all(&d);
}
