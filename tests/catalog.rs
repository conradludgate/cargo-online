use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

const SOURCE: &str = r#"pub const LIMIT: u32 = 1 << 2;
pub fn score(n: u32) -> u32 { 12 - n * 2 }
pub fn guard(n: u32) -> u32 { match n { 1 if n < LIMIT => 4, _ => 2 } }
pub fn flip(n: bool) -> bool { !n }
pub fn iter() -> impl Iterator<Item = u8> { 0..2 }
pub fn opaque() -> impl Copy { 3_u32 }
pub struct Token;
pub fn token() -> Token { Token }
#[derive(Default)]
pub struct Pair { pub left: u32 }
pub fn pair() -> Pair { Pair { left: 5, ..Pair::default() } }
pub fn weak(x: u32) -> bool { x == 5 }
#[test] fn unit() { assert_eq!(score(2),8); }
"#;

// The discovery stub supplies exact source edits and cannot execute mutants.
// The normal CLI must build the real crate and run its actual test targets.
fn setup(root: &Path) {
    fs::create_dir(root.join("src")).unwrap();
    fs::create_dir(root.join("tests")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='sample'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(root.join("src/implementation.rs"), SOURCE).unwrap();
    std::os::unix::fs::symlink("implementation.rs", root.join("src/lib.rs")).unwrap();
    fs::write(
        root.join("tests/check.rs"),
        r#"#[test]fn external(){
        assert_eq!(sample::LIMIT,4); assert_eq!(sample::guard(1),4); assert!(sample::flip(false));
        assert_eq!(sample::iter().count(),2); assert_eq!(sample::pair().left,5);
        std::hint::black_box(sample::weak(5));
    }"#,
    )
    .unwrap();
    let cases = [
        ("constant", "1 << 2", "1 >> 2"),
        ("operator precedence", "n * 2", "n + 2"),
        ("return value", "12 - n * 2", "1"),
        ("guard", "n < LIMIT", "false"),
        ("arm", "1 if n < LIMIT => 4,", ""),
        ("unary", "!n", "n"),
        ("field", "left: 5,", ""),
        ("opaque return", "0..2", "std::iter::empty()"),
        ("opaque inference", "3_u32", "Default::default()"),
        ("ill typed", "{ Token }", "{ Default::default() }"),
        ("survivor", "x == 5", "x != 5"),
    ];
    let mut candidates = Vec::new();
    for (name, original, replacement) in cases {
        let offset = SOURCE.find(original).unwrap();
        let line = SOURCE[..offset].lines().count();
        let col = offset - SOURCE[..offset].rfind('\n').map_or(0, |n| n + 1) + 1;
        candidates.push(json!({"name":name,"package":"sample","file":"src/lib.rs","replacement":replacement,"span":{
            "start":{"line":line,"column":col},"end":{"line":line,"column":col+original.len()}}}));
    }
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&candidates).unwrap(),
    )
    .unwrap();
    let shim = root.join("cargo-mutants");
    fs::write(&shim,"#!/bin/sh\n[ \"$1\" = mutants ] && [ \"$2\" = --list ] && [ \"$3\" = --json ] || exit 1\ncat catalog.json\n").unwrap();
    fs::set_permissions(shim, fs::Permissions::from_mode(0o755)).unwrap();
}

fn command(root: &Path) -> Command {
    let mut paths = vec![root.to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_cargo-online"));
    c.current_dir(root)
        .env("PATH", &path)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_NET_OFFLINE", "true")
        .args(["online", "mutate", "-j2"]);
    c
}

#[test]
fn executes_catalog_with_shared_and_literal_builds() {
    let temp = tempfile::tempdir().unwrap();
    setup(temp.path());
    for (flags, expected) in [
        (
            vec![],
            vec![
                "Killed\tconstant",
                "Killed\toperator precedence",
                "Killed\treturn value",
                "Killed\tguard",
                "Killed\tarm",
                "Killed\tunary",
                "Killed\tfield",
                "Killed\topaque return",
                "Unviable\till typed",
                "Unviable\topaque inference",
                "Survived\tsurvivor",
            ],
        ),
        (
            vec!["--lib"],
            vec!["Killed\treturn value", "Uncovered\tsurvivor"],
        ),
    ] {
        let out = command(temp.path()).args(flags).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        for expected in expected {
            assert!(
                stdout.lines().any(|l| l == expected),
                "{expected}\n{stdout}"
            );
        }
        assert!(stdout.contains("11 catalog mutants"), "{stdout}");
        assert_eq!(
            fs::read_to_string(temp.path().join("src/lib.rs")).unwrap(),
            SOURCE
        );
    }
}

#[test]
fn build_dependency_uses_the_scope_of_the_selected_suite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let manifest = root.join("Cargo.toml");
    let mut content = fs::read_to_string(&manifest).unwrap();
    content.push_str("\n[workspace]\nmembers=['consumer','bridge']\n");
    fs::write(manifest, content).unwrap();
    fs::create_dir_all(root.join("bridge/src")).unwrap();
    fs::write(root.join("bridge/Cargo.toml"),"[package]\nname='bridge'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nrenamed={package='sample',path='..'}\n").unwrap();
    fs::write(
        root.join("bridge/src/lib.rs"),
        "pub fn value()->bool {renamed::weak(5)}",
    )
    .unwrap();
    fs::create_dir_all(root.join("consumer/src")).unwrap();
    fs::write(root.join("consumer/Cargo.toml"), "[package]\nname='consumer'\nversion='0.1.0'\nedition='2021'\n[features]\ngenerated=['dep:bridge']\n[build-dependencies]\nbridge={path='../bridge',optional=true}\n").unwrap();
    fs::write(
        root.join("consumer/build.rs"),
        r#"fn main(){
          #[cfg(feature="generated")] let value=bridge::value();
          #[cfg(not(feature="generated"))] let value=true;
          println!("cargo:rustc-env=VALUE={value}");
        }"#,
    )
    .unwrap();
    fs::write(
        root.join("consumer/src/lib.rs"),
        r#"#[test] fn check(){assert_eq!(env!("VALUE"),"true");}"#,
    )
    .unwrap();
    let mut candidates: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(root.join("catalog.json")).unwrap()).unwrap();
    candidates.retain(|v| v["name"] == "survivor");
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&candidates).unwrap(),
    )
    .unwrap();

    // The integration test links a normal library artifact even in this
    // package-only run. The consumer and its build script are inactive.
    let out = command(root).args(["-p", "sample"]).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("1 in 1 shared builds"), "{stdout}");
    assert!(stdout.contains("Survived\tsurvivor"), "{stdout}");

    let bridge = fs::read_to_string(root.join("bridge/src/lib.rs")).unwrap();
    let start = bridge.find('{').unwrap() + 1;
    candidates.push(json!({"name":"bridge returns false","package":"bridge","file":"bridge/src/lib.rs",
        "replacement":"{ false }","span":{"start":{"line":1,"column":start},"end":{"line":1,"column":bridge.len()+1}}}));
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&candidates).unwrap(),
    )
    .unwrap();
    let disabled = command(root).arg("--test-workspace").output().unwrap();
    assert_eq!(
        disabled.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    let stderr = String::from_utf8(disabled.stderr).unwrap();
    assert_eq!(
        stderr.matches("Workspace baseline passed").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("mutant suite timeout 120.0s"), "{stderr}");
    let stdout = String::from_utf8(disabled.stdout).unwrap();
    assert!(stdout.contains("2 in 2 shared builds"), "{stdout}");

    let out = command(root)
        .args([
            "--timeout",
            "12",
            "--test-workspace",
            "--features",
            "consumer/generated",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("mutant suite timeout 12.0s"), "{stderr}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Killed\tbridge returns false"), "{stdout}");
    assert!(stdout.contains("Killed\tsurvivor"), "{stdout}");
    assert!(stdout.contains("2 literal builds"), "{stdout}");
}
