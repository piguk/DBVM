//! packkit's own tests (packs/packkit/test_packkit.py): building against a fake service. Skipped without python3.
#[test]
fn packkit_unit_tests() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let Ok(out) = std::process::Command::new("python3").args(["-B", "-m", "unittest", "packkit.test_packkit"])
        .env("PYTHONPATH", std::env::join_paths([root.join("packs"), root.join("sdk/python")]).unwrap()).output() else {
        eprintln!("skip: python3 not found");
        return;
    };
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
