use std::{fs, process::Command};

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC with matching rustc-dev libraries"]
fn dev_reuses_only_observed_tests_after_a_covered_body_edit() {
    let compiler = std::env::var_os("CARGO_ONLINE_TEST_RUSTC").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='devtest'\nversion='0.0.0'\nedition='2024'",
    )
    .unwrap();
    let path = root.join("src/lib.rs");
    let original = "pub fn value(x:u8)->u8 { x }\n#[test] fn alpha(){assert_eq!(value(3),3)}\n#[test] fn beta(){}";
    fs::write(&path, original).unwrap();
    let run = || {
        let out = Command::new(env!("CARGO_BIN_EXE_cargo-online"))
            .current_dir(root)
            .env_remove("CARGO_TARGET_DIR")
            .args(["dev", "--lib", "--fast", "--mir-rustc"])
            .arg(&compiler)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    };
    let (pass, _, log) = run();
    assert!(pass && log.contains("Validation finished"), "{log}");
    fs::write(&path, original.replace("{ x }", "{ x + 0 }")).unwrap();
    let (pass, tests, log) = run();
    assert!(
        pass && log.contains("Fast feedback complete")
            && tests.contains("test alpha ... ok")
            && !tests.contains("test beta ..."),
        "{tests}\n{log}"
    );
    fs::write(&path, original.replace("{ x }", "{ x + 1 }")).unwrap();
    let (pass, tests, log) = run();
    assert!(
        !pass && tests.contains("test alpha ... FAILED"),
        "{tests}\n{log}"
    );
    fs::write(&path, format!("{original}\n#[test] fn gamma() {{}}")).unwrap();
    let (pass, tests, log) = run();
    assert!(
        pass && tests.contains("test gamma ... ok") && log.contains("Validation finished"),
        "{tests}\n{log}"
    );
}

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC with matching rustc-dev libraries"]
fn watch_cancels_an_obsolete_test_process() {
    check_cancellation(&[]);
}

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC and cargo-nextest on PATH"]
fn watch_cancels_the_nextest_worker() {
    check_cancellation(&["--nextest"]);
}

fn check_cancellation(flags: &[&str]) {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::time::{Duration, Instant};
    let compiler = std::env::var_os("CARGO_ONLINE_TEST_RUSTC").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='watchtest'\nversion='0.0.0'\nedition='2024'",
    )
    .unwrap();
    let src = root.join("src/lib.rs");
    fs::write(&src,r#"#[test] fn wait() { std::fs::write("target/running", std::process::id().to_string()).unwrap(); std::thread::sleep(std::time::Duration::from_secs(90)); }"#).unwrap();
    let mut watch = Command::new(env!("CARGO_BIN_EXE_cargo-online"))
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .arg("dev")
        .args(flags)
        .args(["--lib", "--watch", "--mir-rustc"])
        .arg(compiler)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let wait_for = |name| {
        let start = Instant::now();
        let p = root.join(name);
        while !p.exists() && start.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(50));
        }
        p.exists()
    };
    let first = wait_for("target/running");
    fs::write(
        &src,
        r#"#[test] fn done() { std::fs::write("target/done", "").unwrap(); }"#,
    )
    .unwrap();
    let next = wait_for("target/done");
    kill(Pid::from_raw(watch.id() as i32), Signal::SIGTERM).unwrap();
    watch.wait().unwrap();
    assert!(first && next, "watch failed to start both revisions");
    let id = fs::read_to_string(root.join("target/running")).unwrap();
    // A reparented zombie may remain until PID 1 reaps it.
    if let Ok(status) = fs::read_to_string(format!("/proc/{id}/status")) {
        assert!(
            status
                .lines()
                .any(|l| l.starts_with("State:") && l.contains("zombie"))
        );
    }
}

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC with matching rustc-dev libraries"]
fn incremental_codegen_reuses_an_unchanged_function_object() {
    use std::{collections::BTreeMap, os::unix::fs::MetadataExt};
    let compiler = std::env::var_os("CARGO_ONLINE_TEST_RUSTC").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='instances'\nversion='0.0.0'\nedition='2024'",
    )
    .unwrap();
    let original = "pub fn eq(x:u8,y:u8)->bool { x == y }\npub fn lt(x:u8,y:u8)->bool { x < y }\n#[test] fn alpha(){assert!(eq(1,1));}\n#[test] fn beta(){assert!(lt(1,2));}";
    let source = root.join("src/lib.rs");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_cargo-online"))
            .current_dir(root)
            .env_remove("CARGO_TARGET_DIR")
            .env("CARGO_INCREMENTAL", "1")
            .args(["dev", "--lib", "--instance-codegen", "--mir-rustc"])
            .arg(&compiler)
            .output()
            .unwrap()
    };
    let objects = || {
        let mut found = BTreeMap::new();
        for e in walkdir::WalkDir::new(root.join("target")) {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy();
            if name.starts_with("co_") && name.ends_with(".o") {
                // rustc may keep two generations. Inspect the newest snapshot.
                let generation = e.path().parent().unwrap().file_name().unwrap().to_owned();
                let stamp = (generation, e.metadata().unwrap().ino());
                let entry = found
                    .entry(name.into_owned())
                    .or_insert_with(|| stamp.clone());
                if stamp > *entry {
                    *entry = stamp;
                }
            }
        }
        found
    };
    fs::write(&source, original).unwrap();
    let control = run();
    assert!(
        control.status.success(),
        "{}",
        String::from_utf8_lossy(&control.stderr)
    );
    let before = objects();
    assert!(before.len() >= 2, "{before:?}");
    fs::write(&source, original.replace("x == y", "x != y")).unwrap();
    let changed = run();
    assert!(
        !changed.status.success()
            && String::from_utf8_lossy(&changed.stdout).contains("alpha ... FAILED")
    );
    let after = objects();
    assert!(
        before
            .iter()
            .any(|(name, (_, inode))| after.get(name).is_some_and(|(_, now)| now == inode)),
        "{before:?} {after:?}"
    );
    assert!(
        before
            .iter()
            .any(|(name, (_, inode))| after.get(name).is_some_and(|(_, now)| now != inode)),
        "{before:?} {after:?}"
    );
}

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC and cargo-nextest on PATH"]
fn nextest_prioritises_a_covered_library_failure() {
    let compiler = std::env::var_os("CARGO_ONLINE_TEST_RUSTC").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='nextestcase'\nversion='0.0.0'\nedition='2024'",
    )
    .unwrap();
    let original = "pub fn eq(x:u8,y:u8)->bool { x == y }\n#[test] fn alpha(){assert!(eq(1,1));}\n#[test] fn beta(){}";
    let source = root.join("src/lib.rs");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_cargo-online"))
            .current_dir(root)
            .env_remove("CARGO_TARGET_DIR")
            .args(["dev", "--lib", "--nextest", "--mir-rustc"])
            .arg(&compiler)
            .output()
            .unwrap()
    };
    fs::write(&source, original).unwrap();
    let baseline = run();
    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    fs::write(&source, original.replace("x == y", "x != y")).unwrap();
    let changed = run();
    let log = String::from_utf8_lossy(&changed.stderr);
    assert_eq!(changed.status.code(), Some(1), "{log}");
    assert!(
        log.contains("Prioritising 1") && log.contains("First failure in") && log.contains("alpha"),
        "{log}"
    );
}

#[test]
#[ignore = "requires CARGO_ONLINE_TEST_RUSTC with matching rustc-dev libraries"]
fn virtual_workspace_prioritises_the_owning_harness() {
    let compiler = std::env::var_os("CARGO_ONLINE_TEST_RUSTC").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['first','second']\nresolver='3'",
    )
    .unwrap();
    let source = "pub fn value(x:u8)->u8 { x }\n#[test] fn same_name() { assert_eq!(value(1),1); }";
    for package in ["first", "second"] {
        let dir = root.join(package);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname='{package}'\nversion='0.0.0'\nedition='2024'"),
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), source).unwrap();
    }
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_cargo-online"))
            .current_dir(root)
            .env_remove("CARGO_TARGET_DIR")
            .args(["dev", "--lib", "--mir-rustc"])
            .arg(&compiler)
            .output()
            .unwrap()
    };
    let control = run();
    assert!(
        control.status.success(),
        "{}",
        String::from_utf8_lossy(&control.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&control.stdout)
            .matches("test same_name ... ok")
            .count(),
        2
    );
    fs::write(
        root.join("second/src/lib.rs"),
        source.replace("{ x }", "{ 0 }"),
    )
    .unwrap();
    let changed = run();
    assert_eq!(
        changed.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let tests = String::from_utf8_lossy(&changed.stdout);
    assert!(
        tests.contains("test same_name ... FAILED") && !tests.contains("test same_name ... ok"),
        "{tests}"
    );
}
