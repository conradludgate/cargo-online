use crate::{
    cargo,
    catalog::{Candidate, Catalog},
    snapshot,
};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use rayon::prelude::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{Duration, Instant},
};

#[derive(Args)]
pub struct Options {
    #[arg(short, long)]
    package: Option<String>,
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: PathBuf,
    #[arg(short,long,default_value_t=std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))]
    jobs: NonZeroUsize,
    /// Maximum number of runtime variants in a build.
    #[arg(long, default_value = "32")]
    batch_size: NonZeroUsize,
    /// Maximum seconds for a test suite invocation.
    #[arg(long, default_value = "120")]
    timeout: NonZeroUsize,
    /// Maximum seconds for one Cargo build, including build scripts.
    #[arg(long, default_value = "300")]
    build_timeout: NonZeroUsize,
    /// List the selected catalog without building.
    #[arg(long)]
    list: bool,
    /// Run tests across the whole workspace for each mutation.
    #[arg(long)]
    test_workspace: bool,
    /// Use only the package's library unit tests, selecting covering tests per function.
    #[arg(long)]
    lib: bool,
    /// Run the whole selected suite. This is already the default outside --lib.
    #[arg(long)]
    full: bool,
    #[arg(long)]
    all_features: bool,
    #[arg(long)]
    no_default_features: bool,
    #[arg(long)]
    features: Vec<String>,
    /// Additional cargo-mutants discovery filters after -- (for example --file src/lib.rs).
    #[arg(last = true)]
    discovery_args: Vec<std::ffi::OsString>,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Status {
    Killed,
    Survived,
    Timeout,
    Unviable,
    Uncovered,
}
struct Engine<'a> {
    options: &'a Options,
    root: &'a Path,
    target: PathBuf,
    package_dir: &'a Path,
    temp: &'a Path,
    catalog: &'a Catalog,
    package: &'a str,
    pool: &'a rayon::ThreadPool,
}
struct Report {
    results: Vec<(usize, Status)>,
    bundled: usize,
    builds: usize,
}
impl Engine<'_> {
    fn command(&self) -> Command {
        let mut c = cargo();
        c.current_dir(self.root)
            .env("CARGO_TARGET_DIR", &self.target)
            .env_remove("CARGO_ONLINE_MUTANT")
            .env_remove("CARGO_ONLINE_TRACE")
            .arg("test");
        if self.options.test_workspace {
            c.arg("--workspace");
        } else {
            c.arg("-p").arg(self.package);
        }
        if self.options.lib {
            c.arg("--lib");
        }
        if self.options.all_features {
            c.arg("--all-features");
        }
        if self.options.no_default_features {
            c.arg("--no-default-features");
        }
        for f in &self.options.features {
            c.arg("--features").arg(f);
        }
        c
    }
    fn build(&self, json: bool) -> Result<(Option<ExitStatus>, String)> {
        let mut cmd = self.command();
        cmd.arg("--no-run");
        if json {
            cmd.arg("--message-format=json");
        }
        crate::runner::execute(
            cmd,
            self.temp,
            Duration::from_secs(self.options.build_timeout.get() as u64),
        )
    }
    fn tests(&self, id: Option<usize>) -> Result<(Option<ExitStatus>, String)> {
        let mut cmd = self.command();
        if let Some(i) = id {
            cmd.env("CARGO_ONLINE_MUTANT", (i + 1).to_string());
        }
        crate::runner::execute(
            cmd,
            self.temp,
            Duration::from_secs(self.options.timeout.get() as u64),
        )
    }
    fn classify(&self, id: Option<usize>) -> Result<Status> {
        let (status, log) = self.tests(id)?;
        match status {
            Some(s) if s.success() => Ok(Status::Survived),
            None => Ok(Status::Timeout),
            Some(_)
                if log.contains("test failed")
                    || log.contains("test result: FAILED")
                    || log.contains("doctest failed") =>
            {
                Ok(Status::Killed)
            }
            _ => bail!("Cargo test infrastructure failed:\n{log}"),
        }
    }
    fn baseline(&self) -> Result<()> {
        let (status, log) = self.build(false)?;
        ensure!(
            status.is_some_and(|s| s.success()),
            "clean build failed or timed out:\n{log}"
        );
        let (s, log) = self.tests(None)?;
        ensure!(
            s.is_some_and(|s| s.success()),
            "baseline failed or timed out:\n{log}"
        );
        Ok(())
    }
    fn literal(&self, id: usize) -> Result<(usize, Status)> {
        self.catalog.literal(self.root, id)?;
        let (build, msg) = self.build(false)?;
        let status = if build.is_none() {
            Status::Timeout
        } else if build.is_some_and(|s| s.success()) {
            self.classify(None)?
        } else {
            ensure!(
                (msg.contains("could not compile")
                    || msg.contains("failed to run custom build command"))
                    && !msg.contains("internal compiler error"),
                "Cargo build failed:\n{msg}"
            );
            Status::Unviable
        };
        self.catalog.restore(self.root, &[id])?;
        Ok((id, status))
    }
    // Diagnostics only select candidates to retry. A literal build determines
    // their actual viability, including failures caused by the dispatcher.
    fn diagnostic_variants(&self, output: &[u8], ids: &[usize]) -> Result<BTreeSet<usize>> {
        let mut suspects = BTreeSet::new();
        let mut sources = BTreeMap::new();
        for line in output.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let Ok(message) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let diag = &message["message"];
            if diag["level"] != "error" {
                continue;
            }
            for span in diag["spans"].as_array().into_iter().flatten() {
                let Some(file) = span["file_name"].as_str() else {
                    continue;
                };
                let path = Path::new(file);
                let relative = path.strip_prefix(self.root).unwrap_or(path);
                if !ids.iter().any(|&i| self.catalog.edits[i].file == relative) {
                    continue;
                }
                let text = match sources.entry(file.to_owned()) {
                    std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(fs::read_to_string(self.root.join(relative))?)
                    }
                };
                if let Some(before) = span["byte_start"]
                    .as_u64()
                    .and_then(|n| text.get(..n as usize))
                    && let Some((_, tail)) = before.rsplit_once("/*cargo-online:")
                    && let Some((number, _)) = tail.split_once("*/")
                    && let Ok(id) = number.parse::<usize>()
                    && ids.contains(&id)
                {
                    suspects.insert(id);
                }
            }
        }
        Ok(suspects)
    }
    fn literals(&self, ids: &[usize]) -> Result<Vec<(usize, Status)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if ids.len() < 4 * self.options.jobs.get().min(4) || self.options.jobs.get() == 1 {
            return ids.iter().map(|&id| self.literal(id)).collect();
        }
        // Keep independent incremental build caches per worker. Cap the number
        // of Cargo builds because each already schedules compiler jobs.
        let chunk = ids.len().div_ceil(self.options.jobs.get().min(4));
        let groups = self.pool.install(|| {
            ids.par_chunks(chunk)
                .enumerate()
                .map(|(n, group)| {
                    let dir = tempfile::Builder::new()
                        .prefix("literal-")
                        .tempdir_in(self.temp)?;
                    snapshot(self.root, dir.path(), &self.target)?;
                    let worker = Engine {
                        root: dir.path(),
                        target: self.target.with_file_name(format!("literal-build-{n}")),
                        ..*self
                    };
                    group
                        .iter()
                        .map(|&id| worker.literal(id))
                        .collect::<Result<Vec<_>>>()
                })
                .collect::<Result<Vec<_>>>()
        })?;
        Ok(groups.into_iter().flatten().collect())
    }
    fn selected(&self, ids: &[usize], messages: &[u8]) -> Result<Vec<(usize, Status)>> {
        use crate::runner::{Harness, Outcome};
        let exe = messages
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .find_map(|v| {
                (v["profile"]["test"] == true)
                    .then(|| crate::json_path(&v["executable"]).ok())
                    .flatten()
            })
            .context("missing library test executable")?;
        let harness = Harness::new(
            exe,
            self.root.join(self.package_dir),
            self.temp,
            Duration::from_secs(self.options.timeout.get() as u64),
        )?;
        let tests = harness.tests()?;
        ensure!(!tests.is_empty(), "no discoverable libtest tests");
        let mut baseline = self.pool.install(|| {
            tests
                .par_iter()
                .enumerate()
                .map(|(n, t)| harness.baseline(n, t, true))
                .collect::<Result<Vec<_>>>()
        })?;
        baseline.sort_by_key(|t| t.elapsed);
        let mut coverage = BTreeMap::new();
        for t in &baseline {
            for &hit in &t.hits {
                coverage.entry(hit).or_insert_with(Vec::new).push(t);
            }
        }
        self.pool.install(|| {
            ids.par_iter()
                .map(|&id| {
                    let (result, _, _) = harness
                        .classify(id + 1, coverage.get(&(id + 1)).map_or(&[], Vec::as_slice))?;
                    Ok((
                        id,
                        match result {
                            Outcome::Killed => Status::Killed,
                            Outcome::Survived => Status::Survived,
                            Outcome::Timeout => Status::Timeout,
                            Outcome::Uncovered => Status::Uncovered,
                        },
                    ))
                })
                .collect()
        })
    }
    fn batch(&self, ids: &[usize], report: &mut Report, literal: &mut Vec<usize>) -> Result<()> {
        self.catalog.bundle(self.root, ids)?;
        // A mutation may be ill-typed or valid only in its original context.
        // Prove viability with its literal edit before reporting it unviable.
        let (status, log) = self.build(true)?;
        let compiled = status.is_some_and(|s| s.success());
        let usable = compiled && self.classify(None)? == Status::Survived;
        let suspects = if compiled {
            BTreeSet::new()
        } else {
            self.diagnostic_variants(log.as_bytes(), ids)?
        };
        if usable {
            let selected = if self.options.lib && !self.options.full && !self.options.test_workspace
            {
                match self.selected(ids, log.as_bytes()) {
                    Ok(outcomes) => Some(outcomes),
                    Err(error) => {
                        eprintln!(
                            "Running the whole Cargo suite after libtest selection failed: {error:#}"
                        );
                        None
                    }
                }
            } else {
                None
            };
            let results = match selected {
                Some(outcomes) => outcomes,
                None => self.pool.install(|| {
                    ids.par_iter()
                        .map(|&i| Ok((i, self.classify(Some(i))?)))
                        .collect::<Result<Vec<_>>>()
                })?,
            };
            report.results.extend(results);
            report.bundled += ids.len();
            report.builds += 1;
        }
        self.catalog.restore(self.root, ids)?;
        if !usable {
            if ids.len() == 1 {
                literal.push(ids[0]);
                return Ok(());
            }
            if !suspects.is_empty() {
                literal.extend(&suspects);
                let remaining: Vec<_> = ids
                    .iter()
                    .copied()
                    .filter(|i| !suspects.contains(i))
                    .collect();
                if !remaining.is_empty() {
                    self.batch(&remaining, report, literal)?;
                }
                return Ok(());
            }
            let (left, right) = ids.split_at(ids.len() / 2);
            self.batch(left, report, literal)?;
            self.batch(right, report, literal)?;
        }
        Ok(())
    }
}

// A build script or procedural macro can observe a variant during compilation,
// before any test process exists. Use source edits for those packages.
fn compile_time_packages(meta: &Value) -> Result<BTreeSet<&str>> {
    let packages = meta["packages"]
        .as_array()
        .context("missing workspace packages")?;
    let mut literal = BTreeSet::new();
    let mut host_dependencies = Vec::new();
    let mut deps = BTreeMap::new();
    for package in packages {
        let name = package["name"].as_str().context("missing package name")?;
        let kinds = package["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|t| t["kind"].as_array().into_iter().flatten());
        for kind in kinds {
            if kind == "proc-macro" {
                host_dependencies.push(name);
            }
            if kind == "custom-build" {
                literal.insert(name);
            }
        }
        for dep in package["dependencies"].as_array().into_iter().flatten() {
            let target = dep["name"].as_str().context("missing dependency name")?;
            if dep["kind"] == "build" {
                host_dependencies.push(target);
            }
            if dep["kind"].is_null() {
                deps.entry(name).or_insert_with(Vec::new).push(target);
            }
        }
    }
    // A regular library can run inside another package's build script.
    let mut visited = BTreeSet::new();
    while let Some(name) = host_dependencies.pop() {
        if visited.insert(name) {
            literal.insert(name);
            if let Some(children) = deps.get(name) {
                host_dependencies.extend(children);
            }
        }
    }
    Ok(literal)
}

pub fn run(options: Options) -> Result<bool> {
    let start = Instant::now();
    let manifest = options.manifest_path.canonicalize()?;
    let out = cargo()
        .current_dir(manifest.parent().unwrap())
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()?;
    ensure!(
        out.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: Value = serde_json::from_slice(&out.stdout)?;
    let original = crate::json_path(&meta["workspace_root"])?;
    let target = crate::json_path(&meta["target_directory"])?;
    let cache = target.join("cargo-online");
    fs::create_dir_all(&cache)?;
    let lock = fs::File::create(cache.join("session.lock"))?;
    lock.lock()?;
    let temp = tempfile::Builder::new()
        .prefix("catalog-")
        .tempdir_in(&cache)?;
    let root = temp.path().join("workspace");
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    snapshot(&original, &root, &target)?;
    let mut list = cargo();
    list.current_dir(&root)
        .args(["mutants", "--list", "--json"]);
    if let Some(p) = &options.package {
        list.arg("-p").arg(p);
    } else if manifest.parent() != Some(&original) {
        list.current_dir(root.join(manifest.parent().unwrap().strip_prefix(&original)?));
    } else {
        list.arg("--workspace");
    }
    let out = list.args(&options.discovery_args).output()?;
    ensure!(
        out.status.success(),
        "cargo-mutants catalog discovery failed (install with cargo install --locked cargo-mutants):\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let candidates: Vec<Candidate> =
        serde_json::from_slice(&out.stdout).context("invalid cargo-mutants catalog")?;
    if options.list {
        for c in candidates {
            println!("{}", c.name);
        }
        return Ok(true);
    }
    let catalog = Catalog::new(&root, candidates)?;
    let mut packages = BTreeMap::<_, Vec<_>>::new();
    for (i, e) in catalog.edits.iter().enumerate() {
        packages.entry(&e.package).or_default().push(i);
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs.get())
        .build()?;
    let mut report = Report {
        results: Vec::new(),
        bundled: 0,
        builds: 0,
    };
    let compile_time = compile_time_packages(&meta)?;
    for (package, ids) in packages {
        let manifest = meta["packages"]
            .as_array()
            .context("missing packages")?
            .iter()
            .find(|p| p["name"] == *package)
            .context("unknown mutation package")?;
        let path = crate::json_path(&manifest["manifest_path"])?;
        let package_dir = path.parent().unwrap().strip_prefix(&original)?;
        let engine = Engine {
            options: &options,
            root: &root,
            target: cache.join("catalog-build"),
            package_dir,
            temp: temp.path(),
            catalog: &catalog,
            package,
            pool: &pool,
        };
        engine.baseline()?;
        let host_code = compile_time.contains(package.as_str());
        let (runtime, mut literal): (Vec<_>, Vec<_>) = ids
            .into_iter()
            .partition(|&i| catalog.bundleable(i) && !host_code);
        eprintln!(
            "{}: {} catalog candidates",
            package,
            runtime.len() + literal.len()
        );
        for batch in runtime.chunks(options.batch_size.get()) {
            engine.batch(batch, &mut report, &mut literal)?;
        }
        report.results.extend(engine.literals(&literal)?);
    }
    report.results.sort_by_key(|&(id, _)| id);
    ensure!(
        report
            .results
            .iter()
            .map(|r| r.0)
            .eq(0..catalog.edits.len()),
        "incomplete catalog results"
    );
    let mut counts = BTreeMap::new();
    for &(id, s) in &report.results {
        *counts.entry(s).or_insert(0usize) += 1;
        println!("{s:?}\t{}", catalog.edits[id].name);
    }
    println!(
        "{} catalog mutants, {} in {} shared builds, {} literal builds; elapsed {:.3}s",
        catalog.edits.len(),
        report.bundled,
        report.builds,
        catalog.edits.len() - report.bundled,
        start.elapsed().as_secs_f64()
    );
    for (status, count) in counts {
        println!("{status:?}: {count}");
    }
    Ok(report
        .results
        .iter()
        .all(|(_, s)| matches!(s, Status::Killed | Status::Unviable)))
}
