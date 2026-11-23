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

#[test]
fn workspace_coverage_keeps_downstream_kills_and_falls_back_for_doctests() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup(root);
    fs::write(root.join("Cargo.toml"), "[package]\nname='sample'\nversion='0.1.0'\nedition='2021'\n[workspace]\nmembers=['sink']\n").unwrap();
    let names = ["integration", "unused", "documented", "exercised"];
    let mut code = String::new();
    let mut catalog = Vec::new();
    for (i, name) in names.into_iter().enumerate() {
        let line = format!("pub fn {name}()->u32 {{ 1 }}\n");
        let col = line.find('1').unwrap() + 1;
        catalog.push(
            json!({"name":name,"package":"sample","file":"src/lib.rs","replacement":"2",
            "span":{"start":{"line":i+1,"column":col},"end":{"line":i+1,"column":col+1}}}),
        );
        code.push_str(&line);
    }
    for name in ["UNIT", "INTEGRATION"] {
        let row = code.lines().count() + 1;
        let line = format!("pub const {name}:u32 = 1;\n");
        let col = line.find('1').unwrap() + 1;
        catalog.push(
            json!({"name":name,"package":"sample","file":"src/lib.rs","replacement":"2",
            "span":{"start":{"line":row,"column":col},"end":{"line":row,"column":col+1}}}),
        );
        code.push_str(&line);
    }
    fs::write(root.join("src/lib.rs"), code).unwrap();
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(root.join("sink/src")).unwrap();
    fs::create_dir_all(root.join("sink/tests")).unwrap();
    fs::write(root.join("sink/Cargo.toml"), "[package]\nname='sink'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nsample={path='..'}\n").unwrap();
    fs::write(
        root.join("sink/src/lib.rs"),
        "/// ```\n/// assert_eq!(sample::documented(), 1);\n/// ```\npub struct Example;\n#[test] fn library(){assert_eq!(sample::UNIT,1); }\n",
    )
    .unwrap();
    fs::write(root.join("sink/tests/downstream.rs"), "#[test] fn check(){std::hint::black_box(sample::exercised());assert_eq!(sample::integration(),1);assert_eq!(sample::INTEGRATION,1);}").unwrap();
    fs::write(root.join("tests/check.rs"), r#"#[test] fn unrelated(){
        if let Ok(id)=std::env::var("CARGO_ONLINE_MUTANT") {
            std::fs::write(std::path::PathBuf::from(std::env::var_os("WITNESS").unwrap()).join(id), "").unwrap();
        }
    }"#).unwrap();
    let out = command(root)
        .env("WITNESS", root)
        .args(["--test-workspace", "--workspace-coverage"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for result in [
        "Killed\tintegration",
        "Uncovered\tunused",
        "Killed\tdocumented",
        "Survived\texercised",
        "Killed\tUNIT",
        "Killed\tINTEGRATION",
        "4 in 1 shared builds",
    ] {
        assert!(stdout.contains(result), "{stdout}");
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr
            .lines()
            .any(|l| l == "Workspace library tests killed UNIT"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Workspace library tests killed INTEGRATION"),
        "{stderr}"
    );
    assert!(
        root.join("3").exists(),
        "doctest hit requires the full suite"
    );
    for id in ["1", "2", "4"] {
        assert!(
            !root.join(id).exists(),
            "unrelated test ran for mutant {id}"
        );
    }
}

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC with matching rustc-dev libraries"]
fn native_catalog_and_source_arms_share_a_harness() {
    let compiler = std::env::var_os("CARGO_ONLINE_TEST_RUSTC").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let content = "// résumé\r\npub fn value(x: u32) -> bool { let y = (|z:u32| z & 3)(x); y < 2 }\n#[test] fn controls() { let f: fn(u32)->bool = value; assert!(f(5)); assert!(!f(6)); }\n";
    fs::write(root.join("src/lib.rs"), content).unwrap();
    let mut cases = Vec::new();
    for (i, (old, new)) in [
        ("&", "|"),
        ("<", ">"),
        ("<", "=="),
        ("let y = (|z:u32| z & 3)(x); y < 2", "true"),
        ("3)", "0u32)"),
    ]
    .iter()
    .enumerate()
    {
        let start = content.lines().nth(1).unwrap().find(old).unwrap() + 1;
        cases.push(json!({"name":format!("edit {i}"),"package":"sample","file":"src/lib.rs","replacement":new,"span":{"start":{"line":2,"column":start},"end":{"line":2,"column":start+old.len()}}}));
    }
    fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&cases).unwrap(),
    )
    .unwrap();
    let out = command(root)
        .args(["--lib", "--mir-rustc"])
        .arg(compiler)
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("4 native MIR edits and 1 source edits in one build"),
        "{stderr}"
    );
    assert_eq!(
        stdout.lines().filter(|s| s.starts_with("Killed\t")).count(),
        5,
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        content
    );
}

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC with matching rustc-dev libraries"]
fn workspace_mir_preserves_downstream_and_shared_source_kills() {
    let compiler = std::env::var_os("CARGO_ONLINE_TEST_RUSTC").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    fs::write(root.join("Cargo.toml"),"[workspace]\nmembers=['consumer']\n[package]\nname='sample'\nversion='0.1.0'\nedition='2024'").unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn selected(x:u8)->bool { x == 3 }\n",
    )
    .unwrap();
    fs::write(root.join("tests/check.rs"), "").unwrap();
    fs::create_dir_all(root.join("consumer/src")).unwrap();
    fs::write(root.join("consumer/Cargo.toml"),"[package]\nname='consumer'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nsample={path='..'}").unwrap();
    fs::write(root.join("catalog.json"), serde_json::to_vec(&[json!({"name":"comparison","package":"sample","file":"src/lib.rs","replacement":"!=","span":{"start":{"line":1,"column":33},"end":{"line":1,"column":35}}})]).unwrap()).unwrap();
    for (import, expected) in [
        ("use sample as subject;", "1 native MIR edits"),
        (
            "#[path=\"../../src/lib.rs\"] mod subject;",
            "1 literal builds",
        ),
    ] {
        fs::write(
            root.join("consumer/src/lib.rs"),
            format!("{import}\n#[test] fn downstream(){{assert!(subject::selected(3));}}"),
        )
        .unwrap();
        let output = command(root)
            .args(["--test-workspace", "--workspace-coverage", "--mir-rustc"])
            .arg(&compiler)
            .output()
            .unwrap();
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.status.success() && log.contains(expected) && log.contains("Killed\tcomparison"),
            "{log}"
        );
    }
}

#[test]
fn a_cached_build_route_never_reuses_an_unviable_verdict() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    fs::write(root.join("src/lib.rs"), format!("{SOURCE}\nmod extra;")).unwrap();
    let extra = root.join("src/extra.rs");
    fs::write(&extra, "").unwrap();
    let path = root.join("catalog.json");
    let mut catalog: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    catalog.retain(|c| c["name"] == "ill typed");
    fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let first = command(root).arg("--lib").output().unwrap();
    assert!(
        first.status.success()
            && String::from_utf8_lossy(&first.stdout).contains("Unviable\till typed")
    );
    fs::write(
        &extra,
        "impl Default for crate::Token { fn default()->Self { Self } }",
    )
    .unwrap();
    let second = command(root).arg("--lib").output().unwrap();
    let output = String::from_utf8_lossy(&second.stdout);
    assert_eq!(second.status.code(), Some(1), "{output}");
    assert!(
        output.contains("Survived\till typed") && output.contains("1 literal builds"),
        "{output}"
    );
}
