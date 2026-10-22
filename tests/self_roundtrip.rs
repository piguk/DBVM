//! SELF conversion and closure round-trip via the built binaries.
use std::process::Command;

fn bin(name: &str) -> String {
    let p = format!("{}/target/release/{}", env!("CARGO_MANIFEST_DIR"), name);
    assert!(std::path::Path::new(&p).exists(), "build release first: {}", p);
    p
}

#[test]
fn static_elf_converts_and_runs() {
    let dir = std::env::temp_dir().join(format!("selfdb-rt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = format!("{}/examples/hello_static.c", env!("CARGO_MANIFEST_DIR"));
    let elf = dir.join("hs");
    assert!(Command::new("gcc").args(["-static", "-no-pie", "-o"]).arg(&elf).arg(&src).status().unwrap().success());
    let out = dir.join("hs.self");
    assert!(Command::new(bin("elf2self")).arg(&elf).arg("-o").arg(&out).status().unwrap().success());
    let run = Command::new(bin("self-exec")).arg(&out).output().unwrap();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "Hello, world!");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn closure_of_ls_is_complete() {
    let dir = std::env::temp_dir().join(format!("selfdb-cl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("c.db");
    let st = Command::new(bin("self")).args(["closure", "/bin/ls"]).arg(&db).output().unwrap();
    assert!(st.status.success());
    let s = String::from_utf8_lossy(&st.stdout);
    assert!(s.contains("missing=0"), "{}", s);
    let _ = std::fs::remove_dir_all(&dir);
}
