//! Content that is itself gzip or zstd (a `.gz` or `.zst` file) does not compress further and is stored as
//! it is; reading it back must give those bytes, not their decompression (which failed the chunk hash and
//! dropped such files from materialized trees, e.g. /lib/apk/db/scripts.tar.gz of Alpine).
use selfdb::vm;
use std::io::Write;

fn noise(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e3779b97f4a7c15;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

#[test]
fn compressed_files_read_back_unchanged() {
    let d = std::env::temp_dir().join(format!("as-is-{}", std::process::id()));
    let _ = vm::remove_tree(&d);
    std::fs::create_dir_all(&d).unwrap();
    let c = vm::init_vm_db_with_opts(d.join("v.db").to_str().unwrap(), true, true).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&noise(3000)).unwrap();
    let gz = gz.finish().unwrap();
    let zst = zstd::encode_all(&noise(200_000)[..], 3).unwrap();
    for (p, data) in [("/a.tar.gz", &gz), ("/b.zst", &zst)] {
        vm::vm_add_bytes(&c, p, data, 0o644, 1).unwrap();
        assert_eq!(&vm::vm_cat(&c, p).unwrap(), data, "{}", p);
    }
    let tree = d.join("tree");
    vm::vm_materialize_tree(&c, &tree).unwrap();
    assert_eq!(std::fs::read(tree.join("a.tar.gz")).unwrap(), gz);
    assert_eq!(std::fs::read(tree.join("b.zst")).unwrap(), zst);
    let _ = vm::remove_tree(&d);
}
