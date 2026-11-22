use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn fixture(root: &Path, source: &str) {
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='sample'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), source).unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-online"))
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_NET_OFFLINE", "true")
        .args(["online", "mutate", "-j2"])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn classifies_mutants_against_the_same_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"#![no_std]
mod nested;
pub const fn constant() -> bool { 1 == 1 }
pub fn unused(x: u8) -> bool { x < 5 }
#[test] fn covered() { let _ = nested::weak(2); assert!(nested::strong(1)); }
#[test] #[ignore] fn ignored() { panic!() }
"#;
    fixture(dir.path(), source);
    fs::write(
        dir.path().join("src/nested.rs"),
        "pub fn weak(x:u8)->bool{x>0}\npub fn strong(x:u8)->bool{x==1}\n",
    )
    .unwrap();
    for (args, expected) in [
        (&[][..], "1 killed, 1 survived, 1 uncovered"),
        (&["--full"][..], "1 killed, 2 survived, 0 uncovered"),
    ] {
        let out = run(dir.path(), args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains(expected), "{stdout}");
        assert!(stdout.contains("3 mutants, 1 tests"), "{stdout}");
        assert_eq!(
            fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            source
        );
    }
}

#[test]
fn baseline_failure_stops_classification() {
    let dir = tempfile::tempdir().unwrap();
    fixture(
        dir.path(),
        "fn f()->bool{1==2}\n#[test]fn broken(){assert!(f())}",
    );
    let out = run(dir.path(), &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("baseline test broken failed"));
}

#[test]
fn bounds_execution_of_a_hanging_mutant() {
    let dir = tempfile::tempdir().unwrap();
    fixture(
        dir.path(),
        "fn f(){while 1==2 {std::thread::yield_now();}}\n#[test]fn returns(){f()}",
    );
    let out = run(dir.path(), &["--timeout", "1"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 timed out"));
}
