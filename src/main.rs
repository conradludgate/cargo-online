mod catalog;
mod engine;
mod host;
mod instrument;
mod runner;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use serde_json::Value;
use std::{
    env, fs,
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Duration, Instant},
};
use walkdir::WalkDir;

#[derive(Parser)]
#[command(name = "cargo online", version)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Run the cargo-mutants catalog using shared builds.
    Mutate(engine::Options),
    /// Compile comparison mutants together and run affected library unit tests.
    MutateComparisons(Comparisons),
}

#[derive(clap::Args)]
struct Comparisons {
    #[arg(short, long)]
    package: Option<String>,
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: PathBuf,
    /// Maximum concurrent test processes.
    #[arg(short, long, default_value_t = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))]
    jobs: NonZeroUsize,
    /// Time limit in seconds per test process.
    #[arg(long, default_value = "60")]
    timeout: NonZeroUsize,
    /// Try every test for each mutant, including tests without observed coverage.
    #[arg(long)]
    full: bool,
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}
fn json_path(value: &Value) -> Result<PathBuf> {
    Ok(PathBuf::from(value.as_str().context("missing Cargo path")?))
}
fn snapshot(root: &Path, dest: &Path, target: &Path) -> Result<()> {
    for entry in WalkDir::new(root)
        .min_depth(1)
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
        let copy = dest.join(path.strip_prefix(root)?);
        if entry.file_type().is_dir() {
            fs::create_dir(&copy)?;
        } else if entry.file_type().is_file() {
            fs::copy(path, copy)?;
        } else if entry.file_type().is_symlink() {
            let resolved = path
                .canonicalize()
                .with_context(|| format!("resolving {}", path.display()))?;
            let relative = resolved.strip_prefix(root).with_context(|| {
                format!(
                    "workspace symlink points outside snapshot: {}",
                    path.display()
                )
            })?;
            std::os::unix::fs::symlink(dest.join(relative), copy)?;
        } else {
            bail!("unsupported workspace file {}", path.display());
        }
    }
    Ok(())
}

fn run(action: Action) -> Result<bool> {
    match action {
        Action::Mutate(options) => engine::run(options),
        Action::MutateComparisons(options) => comparisons(options),
    }
}
fn comparisons(options: Comparisons) -> Result<bool> {
    let Comparisons {
        package,
        manifest_path,
        jobs,
        timeout,
        full,
    } = options;
    let start = Instant::now();
    let manifest = manifest_path.canonicalize()?;
    let output = cargo()
        .args([
            "metadata",
            "--format-version=1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()?;
    ensure!(
        output.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let meta: Value = serde_json::from_slice(&output.stdout)?;
    let root = json_path(&meta["workspace_root"])?;
    let target = json_path(&meta["target_directory"])?;
    let packages = meta["packages"].as_array().context("missing packages")?;
    let pkg = match package {
        Some(ref name) => packages.iter().find(|p| p["name"] == name.as_str()),
        None if packages.len() == 1 => packages.first(),
        None => packages
            .iter()
            .find(|p| json_path(&p["manifest_path"]).ok().as_ref() == Some(&manifest)),
    }
    .context("select a workspace package with --package NAME")?;
    let lib = pkg["targets"]
        .as_array()
        .context("missing targets")?
        .iter()
        .find(|t| {
            t["kind"]
                .as_array()
                .is_some_and(|ks| ks.iter().any(|k| k == "lib" || k == "rlib"))
        })
        .context("selected package needs a library target")?;
    let lib = json_path(&lib["src_path"])?;
    let lib = lib.strip_prefix(&root)?;
    let pkg_manifest = json_path(&pkg["manifest_path"])?;
    let pkg_dir = pkg_manifest.parent().unwrap().strip_prefix(&root)?;
    let cache = target.join("cargo-online");
    fs::create_dir_all(&cache)?;
    // Cargo may replace the binary once its build lock is released.
    let lock = fs::File::create(cache.join("session.lock"))?;
    lock.lock()?;
    let temp = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(&cache)?;
    let copy = temp.path().join("workspace");
    fs::create_dir(&copy)?;
    snapshot(&root, &copy, &target)?;
    let mut sites = Vec::new();
    instrument::instrument(&copy, lib, lib.parent().unwrap(), &mut sites)?;
    ensure!(!sites.is_empty(), "no comparison mutation sites found");
    fs::OpenOptions::new()
        .append(true)
        .open(copy.join(lib))?
        .write_all(
            instrument::RUNTIME
                .replace("SITE_COUNT", &(sites.len() + 1).to_string())
                .as_bytes(),
        )?;
    let cwd = copy.join(pkg_dir);
    let out = cargo()
        .current_dir(&cwd)
        .args([
            "test",
            "--lib",
            "--no-run",
            "--message-format=json-render-diagnostics",
            "-p",
        ])
        .arg(pkg["name"].as_str().unwrap())
        .env("CARGO_TARGET_DIR", cache.join("build"))
        .output()?;
    ensure!(
        out.status.success(),
        "instrumented build failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let exe = String::from_utf8(out.stdout)?
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|v| {
            (v["profile"]["test"] == true)
                .then(|| json_path(&v["executable"]).ok())
                .flatten()
        })
        .context("Cargo did not produce a libtest executable")?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.get())
        .build()?;
    let harness = runner::Harness::new(
        exe,
        cwd,
        temp.path(),
        Duration::from_secs(timeout.get() as u64),
    )?;
    let tests = harness.tests()?;
    ensure!(!tests.is_empty(), "no runnable library unit tests");
    let mut baseline: Vec<_> = pool.install(|| {
        tests
            .par_iter()
            .enumerate()
            .map(|(i, t)| harness.baseline(i, t, !full))
            .collect::<Result<_>>()
    })?;
    baseline.sort_by_key(|test| test.elapsed);
    let all: Vec<_> = baseline.iter().collect();
    let mut by_site = vec![Vec::new(); sites.len() + 1];
    for test in &baseline {
        for &id in &test.hits {
            by_site
                .get_mut(id)
                .context("unexpected site in baseline trace")?
                .push(test);
        }
    }
    let setup = start.elapsed();
    let work = Instant::now();
    let outcomes = pool.install(|| {
        (1..=sites.len())
            .into_par_iter()
            .map(|id| harness.classify(id, if full { &all } else { &by_site[id] }))
            .collect::<Result<Vec<_>>>()
    })?;
    let mut counts = [0; 4];
    let mut executions = 0;
    for (site, (outcome, runs, witness)) in sites.iter().zip(outcomes) {
        println!(
            "{}:{}:{}  {} {}",
            site.file.display(),
            site.line,
            site.column,
            outcome.label(),
            witness.unwrap_or("")
        );
        counts[outcome as usize] += 1;
        executions += runs;
    }
    let secs = work.elapsed().as_secs_f64();
    let total = start.elapsed().as_secs_f64();
    println!(
        "{} mutants, {} tests, {} mutant test executions; {} killed, {} survived, {} uncovered, {} timed out",
        sites.len(),
        tests.len(),
        executions,
        counts[0],
        counts[1],
        counts[2],
        counts[3]
    );
    println!(
        "build + baseline {:.3}s, mutation {:.3}s, total {:.3}s; {:.2} killed or survived / total second",
        setup.as_secs_f64(),
        secs,
        total,
        (counts[0] + counts[1]) as f64 / total
    );
    Ok(counts[1..].iter().all(|c| *c == 0))
}

fn main() -> ExitCode {
    let mut args: Vec<_> = env::args_os().collect();
    if args.get(1).is_some_and(|s| s == "online") {
        args.remove(1);
    }
    match run(Cli::parse_from(args).command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("cargo-online: {e:#}");
            ExitCode::from(2)
        }
    }
}
