//! Edit feedback: use the previous successful library run to order the next one.
use crate::{cargo, json_path, mir::Toolchain};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use walkdir::WalkDir;
#[derive(Args)]
pub struct Options {
    /// rustc executable with the matching private compiler libraries.
    #[arg(long)]
    mir_rustc: PathBuf,
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: PathBuf,
    #[arg(short, long)]
    package: Option<String>,
    /// Track library test harnesses across the workspace.
    #[arg(long, conflicts_with = "package")]
    workspace: bool,
    /// Validate library unit tests only. The default validates the workspace.
    #[arg(long)]
    lib: bool,
    /// Run the full validation immediately, without prioritising observed tests.
    #[arg(long)]
    full: bool,
    /// Stop after observed library tests pass for an isolated, covered function-body edit.
    #[arg(long, requires = "lib", conflicts_with = "full")]
    fast: bool,
    /// Use cargo-nextest process isolation for the selected library tests.
    #[arg(long, requires = "lib")]
    nextest: bool,
    /// Give eligible monomorphised functions individual incremental codegen units.
    #[arg(long)]
    instance_codegen: bool,
    /// Rebuild after edits and cancel superseded runs.
    #[arg(long)]
    watch: bool,
    #[arg(long)]
    all_features: bool,
    #[arg(long)]
    no_default_features: bool,
    #[arg(long)]
    features: Vec<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    bodies: BTreeMap<u64, u64>,
    hits: BTreeMap<u64, BTreeSet<String>>,
    failed: Vec<String>,
    #[serde(default)]
    structure: Option<u64>,
}
fn files(root: &Path, target: &Path, include_lock: bool) -> Result<u64> {
    let lock = root.join("Cargo.lock");
    let mut h = DefaultHasher::new();
    for entry in WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.path() != target
                && ![".git", ".scratch", "target"]
                    .iter()
                    .any(|n| e.file_name() == *n)
        })
    {
        let e = entry?;
        if !include_lock && e.path() == lock {
            continue;
        }
        if e.file_type().is_file() || e.file_type().is_symlink() {
            e.path().hash(&mut h);
            let m = fs::metadata(e.path())?;
            m.len().hash(&mut h);
            m.modified()?.hash(&mut h);
        }
    }
    Ok(h.finish())
}
// Only ordinary top-level library functions qualify for a partial validation.
// Hash everything else, including manifests, tests, signatures and local dependencies.
fn structure(root: &Path, target: &Path, source: &Path, exe: &Path) -> Result<u64> {
    use syn::spanned::Spanned;
    let mut hash = DefaultHasher::new();
    exe.hash(&mut hash);
    for entry in WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.path() != target
                && ![".git", ".scratch", "target"]
                    .iter()
                    .any(|n| e.file_name() == *n)
        })
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().is_dir() {
            continue;
        }
        ensure!(fs::metadata(path)?.is_file(), "non-file workspace input");
        path.hash(&mut hash);
        let bytes = fs::read(path)?;
        if path == source
            && let Ok(text) = std::str::from_utf8(&bytes)
            && !text.starts_with('\u{feff}')
            && let Ok(file) = syn::parse_file(text)
            && file.shebang.is_none()
        {
            let mut end = 0;
            for item in file.items {
                if let syn::Item::Fn(f) = item
                    && f.attrs.is_empty()
                    && f.sig.constness.is_none()
                {
                    let span = f.block.span().byte_range();
                    bytes[end..span.start].hash(&mut hash);
                    end = span.end;
                }
            }
            bytes[end..].hash(&mut hash);
        } else {
            bytes.hash(&mut hash);
        }
    }
    Ok(hash.finish())
}
// The watch process owns exactly one revision's process tree. A runner
// can assign its tests their own process groups. Linux reparents these
// children to our subreaper after their immediate parent exits.
fn reap_revision() -> Result<()> {
    use nix::{
        sys::{
            signal::{Signal, kill},
            wait::waitpid,
        },
        unistd::Pid,
    };
    let path = format!("/proc/self/task/{}/children", std::process::id());
    loop {
        let children = fs::read_to_string(&path)?;
        if children.trim().is_empty() {
            return Ok(());
        }
        for id in children.split_whitespace() {
            let pid = Pid::from_raw(id.parse()?);
            let _ = kill(pid, Signal::SIGKILL);
            waitpid(pid, None)?;
        }
    }
}
fn watch(root: &Path, target: &Path) -> Result<bool> {
    use std::os::unix::process::CommandExt;
    nix::sys::prctl::set_child_subreaper(true)?;
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = stopped.clone();
    ctrlc::set_handler(move || signal.store(true, std::sync::atomic::Ordering::Relaxed))?;
    let args: Vec<_> = std::env::args_os()
        .skip(1)
        .filter(|s| s != "--watch")
        .collect();
    loop {
        let rev = files(root, target, true)?;
        let mut child = Command::new(std::env::current_exe()?)
            .args(&args)
            .process_group(0)
            .spawn()?;
        let mut done = false;
        loop {
            if !done && child.try_wait()?.is_some() {
                done = true;
            }
            let updated = files(root, target, true);
            let stopping = stopped.load(std::sync::atomic::Ordering::Relaxed);
            if stopping || updated.as_ref().ok() != Some(&rev) {
                if !done {
                    use nix::sys::signal::{Signal, killpg};
                    let group = nix::unistd::Pid::from_raw(child.id() as i32);
                    let _ = killpg(group, Signal::SIGKILL);
                    child.wait()?;
                }
                reap_revision()?;
                if stopping {
                    return Ok(false);
                }
                updated?;
                break;
            }
            std::thread::sleep(Duration::from_millis(200))
        }
    }
}
fn read_bodies(path: &Path) -> Result<BTreeMap<u64, u64>> {
    fs::read_to_string(path)?
        .lines()
        .map(|line| {
            let (a, b) = line.split_once('\t').context("invalid driver body index")?;
            Ok((a.parse()?, b.parse()?))
        })
        .collect()
}
fn save(path: &Path, state: &State) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(tmp.as_file_mut(), state)?;
    tmp.as_file_mut().flush()?;
    tmp.persist(path)?;
    Ok(())
}
pub fn run(o: Options) -> Result<bool> {
    let start = Instant::now();
    let manifest = o.manifest_path.canonicalize()?;
    let m = cargo()
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()?;
    ensure!(
        m.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&m.stderr)
    );
    let meta: Value = serde_json::from_slice(&m.stdout)?;
    let root = json_path(&meta["workspace_root"])?;
    let target = json_path(&meta["target_directory"])?;
    if !root.join("Cargo.lock").exists() {
        let status = cargo()
            .args(["metadata", "--format-version=1", "--manifest-path"])
            .arg(&manifest)
            .stdout(std::process::Stdio::null())
            .status()?;
        ensure!(
            status.success(),
            "resolving the initial workspace lockfile failed"
        );
    }
    let packages = meta["packages"].as_array().context("missing packages")?;
    let focus = if o.workspace {
        None
    } else if let Some(name) = &o.package {
        Some(
            packages
                .iter()
                .find(|p| p["name"] == *name)
                .context("unknown workspace package")?,
        )
    } else {
        packages
            .iter()
            .find(|p| p["manifest_path"].as_str() == manifest.to_str())
    };
    let mut libraries = BTreeMap::new();
    for package in packages
        .iter()
        .filter(|p| focus.is_none_or(|f| f["id"] == p["id"]))
    {
        if let Some(lib) = package["targets"]
            .as_array()
            .context("missing Cargo targets")?
            .iter()
            .find(|t| {
                t["kind"].as_array().is_some_and(|kinds| {
                    kinds.iter().any(|k| {
                        k == "lib"
                            || k == "rlib"
                            || k == "proc-macro"
                            || k == "cdylib"
                            || k == "staticlib"
                            || k == "dylib"
                    })
                })
            })
        {
            libraries.insert(
                package["id"].as_str().context("missing package ID")?,
                (
                    json_path(&lib["src_path"])?,
                    package["name"].as_str().context("missing package name")?,
                ),
            );
        }
    }
    ensure!(
        !libraries.is_empty(),
        "no library test harness in the selected packages"
    );
    ensure!(
        !o.fast || libraries.len() == 1,
        "select a single library with -p PACKAGE for --fast"
    );
    let source_paths = std::env::join_paths(libraries.values().map(|(src, _)| src))?;
    let scope = |c: &mut Command| {
        if let Some(package) = focus {
            c.arg("-p").arg(package["name"].as_str().unwrap());
        } else {
            c.arg("--workspace");
        }
    };
    let cache = target.join("cargo-online");
    let tc = Toolchain::prepare(&o.mir_rustc, &cache)?;
    if o.watch {
        return watch(&root, &target);
    }
    let mut h = DefaultHasher::new();
    (
        &tc.directory,
        &root,
        &libraries,
        o.lib,
        o.instance_codegen,
        o.nextest,
        o.all_features,
        o.no_default_features,
        &o.features,
    )
        .hash(&mut h);
    let dir = cache.join(format!("dev-{:016x}", h.finish()));
    fs::create_dir_all(&dir)?;
    let lock = fs::File::create(dir.join("session.lock"))?;
    lock.lock()?;
    let before_build = files(&root, &target, false)?;
    let state_path = dir.join("history.json");
    let mut state: State = fs::read(&state_path)
        .ok()
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or_default();
    let report = dir.join("bodies");
    let command = |nextest: bool| {
        let mut c = cargo();
        if nextest {
            c.args([
                "nextest",
                "run",
                "--message-format",
                "libtest-json",
                "--message-format-version",
                "0.1",
            ])
            .env("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1");
        } else {
            c.arg("test");
        }
        c.current_dir(&root)
            .arg("--manifest-path")
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", dir.join("build"))
            .env_remove("CARGO_ONLINE_DEV_TRACE")
            .env_remove("CARGO_ONLINE_MUTANT");
        tc.instrument(&mut c, &tc.wrapper(), &root, &report);
        if o.instance_codegen {
            c.env("CARGO_ONLINE_INSTANCE_CODEGEN", "1");
        }
        c.env("CARGO_ONLINE_MIR_SOURCES", &source_paths)
            .env("CARGO_ONLINE_MIR_DEV", "1");
        if o.all_features {
            c.arg("--all-features");
        }
        if o.no_default_features {
            c.arg("--no-default-features");
        }
        for f in &o.features {
            c.arg("--features").arg(f);
        }
        c
    };
    let mut c = command(false);
    scope(&mut c);
    let build = c
        .args([
            "--lib",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .stderr(std::process::Stdio::inherit())
        .output()?;
    ensure!(build.status.success(), "development test build failed");
    ensure!(
        files(&root, &target, false)? == before_build,
        "sources changed during the development build"
    );
    let rev = files(&root, &target, true)?;
    let mut binaries = BTreeMap::new();
    let mut bodies = BTreeMap::new();
    let mut owners = BTreeMap::new();
    for message in build
        .stdout
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
    {
        if message["profile"]["test"] != true || message["executable"].is_null() {
            continue;
        }
        let source = json_path(&message["target"]["src_path"])?;
        let Some((id, (src, package))) = message["package_id"]
            .as_str()
            .and_then(|id| libraries.get_key_value(id))
        else {
            continue;
        };
        if &source != src {
            continue;
        }
        let exe = json_path(&message["executable"])?;
        let index = read_bodies(&report.with_file_name(format!(
            "bodies-{}",
            exe.file_name().unwrap().to_string_lossy()
        )))?;
        for (id, hash) in index {
            ensure!(
                bodies.insert(id, hash).is_none(),
                "duplicate compiler definition identity across test harnesses"
            );
            owners.insert(id, *package);
        }
        binaries.insert(*id, (source, exe));
    }
    ensure!(
        binaries.len() == libraries.len(),
        "missing development harness"
    );
    let structure = if o.fast {
        let (src, exe) = binaries.values().next().unwrap();
        structure(&root, &target, src, exe).ok()
    } else {
        None
    };
    let covered_edit = structure.is_some()
        && structure == state.structure
        && state.bodies.keys().eq(bodies.keys())
        && bodies != state.bodies
        && bodies
            .iter()
            .filter(|(id, h)| state.bodies.get(id) != Some(h))
            .all(|(id, _)| state.hits.get(id).is_some_and(|tests| !tests.is_empty()));
    let mut names = state.failed.clone();
    let mut affected = BTreeSet::new();
    for (id, hash) in &bodies {
        if state.bodies.get(id) != Some(hash)
            && let Some(hits) = state.hits.get(id)
        {
            affected.insert(owners[id]);
            names.extend(hits.iter().cloned());
        }
    }
    let mut first_result = true;
    let mut seen = BTreeSet::new();
    names.retain(|n| seen.insert(n.clone()));
    if !o.full && !names.is_empty() {
        let mut c = command(o.nextest);
        if state.failed.is_empty() {
            for name in affected {
                c.arg("-p").arg(name);
            }
        } else {
            scope(&mut c);
        }
        c.args(["--lib", "--", "--exact"]).args(&names);
        eprintln!(
            "Prioritising {} observed or previously failing tests",
            names.len()
        );
        let (passed, failed, finished) = run_tests(&mut c, start, &mut first_result, o.nextest)?;
        eprintln!(
            "Prioritised tests finished in {:.3}s from edit check",
            start.elapsed().as_secs_f64()
        );
        if !passed {
            state.failed = failed;
            ensure!(
                files(&root, &target, true)? == rev,
                "workspace changed during selected tests"
            );
            save(&state_path, &state)?;
            return Ok(false);
        }
        if covered_edit && names.iter().all(|n| finished.contains(n)) {
            ensure!(
                files(&root, &target, true)? == rev,
                "workspace changed during selected tests"
            );
            eprintln!("Fast feedback complete; coverage history is from the last full library run");
            return Ok(true);
        }
    }
    let trace = tempfile::NamedTempFile::new_in(&dir)?;
    let mut c = command(o.nextest);
    if o.lib {
        scope(&mut c);
        c.arg("--lib");
    } else {
        c.arg("--workspace");
    }
    c.env("CARGO_ONLINE_DEV_TRACE", trace.path());
    let (passed, failed, finished) = run_tests(&mut c, start, &mut first_result, o.nextest)?;
    if files(&root, &target, true)? != rev {
        bail!("workspace changed during validation; feedback index was not updated")
    }
    if passed {
        let mut hits: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
        for line in fs::read_to_string(trace.path())?.lines() {
            if let Some((id, name)) = line.split_once('\t')
                && let Ok(id) = id.parse()
                && finished.contains(name)
            {
                hits.entry(id).or_default().insert(name.into());
            }
        }
        state = State {
            bodies,
            hits,
            structure,
            failed: Vec::new(),
        };
    } else {
        state.failed = failed;
    }
    save(&state_path, &state)?;
    eprintln!(
        "Validation finished in {:.3}s",
        start.elapsed().as_secs_f64()
    );
    Ok(passed)
}
fn run_tests(
    command: &mut Command,
    start: Instant,
    first_result: &mut bool,
    nextest: bool,
) -> Result<(bool, Vec<String>, BTreeSet<String>)> {
    use std::io::{BufRead, BufReader};
    let mut child = command.stdout(std::process::Stdio::piped()).spawn()?;
    let mut failed = Vec::new();
    let mut finished = BTreeSet::new();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut line = Vec::new();
    while lines.read_until(b'\n', &mut line)? != 0 {
        let text = String::from_utf8_lossy(&line);
        let event = if nextest {
            let event: Value =
                serde_json::from_slice(&line).context("invalid nextest JSON event")?;
            match (
                event["type"].as_str(),
                event["event"].as_str(),
                event["name"].as_str(),
            ) {
                (Some("test"), Some(result @ ("ok" | "failed")), Some(name)) => {
                    let (_, name) = name
                        .split_once('$')
                        .context("missing nextest binary identity")?;
                    Some((name.to_owned(), result == "failed"))
                }
                _ => None,
            }
        } else {
            std::io::stdout().write_all(&line)?;
            text.trim_end()
                .strip_prefix("test ")
                .and_then(|s| s.rsplit_once(" ... "))
                .filter(|(_, result)| matches!(*result, "ok" | "FAILED"))
                .map(|(name, result)| (name.to_owned(), result == "FAILED"))
        };
        if let Some((name, did_fail)) = event {
            if *first_result {
                *first_result = false;
                eprintln!("First test result in {:.3}s", start.elapsed().as_secs_f64());
            }
            finished.insert(name.clone());
            if did_fail {
                if failed.is_empty() {
                    eprintln!("First failure in {:.3}s", start.elapsed().as_secs_f64());
                }
                failed.push(name);
            }
        }
        line.clear();
    }
    Ok((child.wait()?.success(), failed, finished))
}
