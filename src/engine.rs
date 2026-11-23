use crate::{
    cargo,
    catalog::{Candidate, Catalog},
    snapshot,
    workspace_coverage::{Harnesses, harnesses},
};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use rayon::prelude::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{Duration, Instant},
};

#[derive(Args)]
pub struct Options {
    /// Use the specified rustc-dev compiler for MIR bundles and catalog fallbacks.
    #[arg(long)]
    mir_rustc: Option<PathBuf>,
    #[arg(short, long)]
    package: Option<String>,
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: PathBuf,
    #[arg(short,long,default_value_t=std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))]
    jobs: NonZeroUsize,
    /// Maximum number of runtime variants in a build.
    #[arg(long, default_value = "32")]
    batch_size: NonZeroUsize,
    /// Maximum seconds per suite. Workspace runs default to twice the clean suite time (minimum 120s).
    #[arg(long)]
    timeout: Option<NonZeroUsize>,
    /// Maximum seconds for one Cargo build, including build scripts.
    #[arg(long, default_value = "300")]
    build_timeout: NonZeroUsize,
    /// List the selected catalog without building.
    #[arg(long)]
    list: bool,
    /// Print confirmed outcomes as they become available.
    #[arg(long, conflicts_with = "list")]
    stream: bool,
    /// Run tests across the whole workspace for each mutation.
    #[arg(long)]
    test_workspace: bool,
    /// Select workspace test targets using function-entry coverage from the bundle baseline.
    #[arg(long, requires = "test_workspace", conflicts_with = "full")]
    workspace_coverage: bool,
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
impl Options {
    fn feature_args(&self, c: &mut Command) {
        if self.all_features {
            c.arg("--all-features");
        }
        if self.no_default_features {
            c.arg("--no-default-features");
        }
        for f in &self.features {
            c.arg("--features").arg(f);
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Status {
    Killed,
    Survived,
    Timeout,
    Unviable,
    Uncovered,
}
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const WORKSPACE_BASELINE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

struct Baseline {
    libraries_first: bool,
    artifacts: Option<crate::host::Build>,
    timeout: Duration,
}

struct Engine<'a> {
    compiler: Option<&'a crate::mir::Toolchain>,
    plan: Option<&'a Path>,
    session_driver: Option<&'a crate::mir::SessionDriver>,
    libraries_first: bool,
    timeout: Duration,
    options: &'a Options,
    root: &'a Path,
    target: PathBuf,
    package_dir: &'a Path,
    temp: &'a Path,
    catalog: &'a Catalog,
    package: &'a str,
    pool: &'a rayon::ThreadPool,
}
// These hints choose a build strategy only. Each invocation still compiles
// every batch and executes or otherwise classifies every catalog entry.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Routes {
    source: BTreeSet<usize>,
    literal: BTreeSet<usize>,
}
struct Report {
    routes: Routes,
    results: Vec<(usize, Status)>,
    bundled: usize,
    builds: usize,
}
impl Engine<'_> {
    fn completed(&self, id: usize, status: Status) -> Result<(usize, Status)> {
        if self.options.stream {
            let mut output = io::stdout().lock();
            writeln!(output, "{status:?}\t{}", self.catalog.edits[id].name)?;
            output.flush()?;
        }
        Ok((id, status))
    }

    fn command(&self) -> Command {
        let mut c = cargo();
        if let Some(tc) = self.compiler {
            tc.compiler(&mut c);
            if let (Some(plan), Some(driver)) = (self.plan, self.session_driver) {
                tc.instrument(
                    &mut c,
                    &driver.path().join("rustc"),
                    &self.root.join(self.package_dir),
                    &plan.join("report"),
                );
                c.env("CARGO_ONLINE_MIR_PLAN", plan.join("sites"))
                    .env(
                        format!("CARGO_ONLINE_PLAN_{}", self.package),
                        plan.join("sites"),
                    )
                    .env(
                        "CARGO_ONLINE_MIR_SOURCE_INDEX",
                        driver.path().join("sources"),
                    )
                    .env("CARGO_INCREMENTAL", "1");
            }
        }
        c.current_dir(self.root)
            .env("CARGO_TARGET_DIR", &self.target)
            .env_remove("CARGO_ONLINE_MUTANT")
            .env_remove("CARGO_ONLINE_TRACE")
            .env_remove("CARGO_ONLINE_TRACE_TARGETS")
            .arg("test");
        if self.options.test_workspace {
            c.arg("--workspace");
        } else {
            c.arg("-p").arg(self.package);
        }
        if self.options.lib {
            c.arg("--lib");
        }
        self.options.feature_args(&mut c);
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
    fn tests(&self, id: Option<usize>, timeout: Duration) -> Result<(Option<ExitStatus>, String)> {
        let mut cmd = self.command();
        if let Some(i) = id {
            cmd.env("CARGO_ONLINE_MUTANT", (i + 1).to_string());
        }
        crate::runner::execute(cmd, self.temp, timeout)
    }
    fn classify(&self, id: Option<usize>) -> Result<Status> {
        Self::outcome(self.tests(id, self.timeout)?)
    }
    fn outcome((status, log): (Option<ExitStatus>, String)) -> Result<Status> {
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
    fn baseline(&self) -> Result<Baseline> {
        let (status, log) = self.build(true)?;
        ensure!(
            status.is_some_and(|s| s.success()),
            "clean build failed or timed out:\n{log}"
        );
        let artifacts = crate::host::Build::from_messages(&log).ok();
        let build_log = log;
        let adaptive = self.options.test_workspace && self.options.timeout.is_none();
        let start = Instant::now();
        let (s, log) = self.tests(
            None,
            if adaptive {
                WORKSPACE_BASELINE_TIMEOUT
            } else {
                self.timeout
            },
        )?;
        let elapsed = start.elapsed();
        ensure!(
            s.is_some_and(|s| s.success()),
            "baseline failed or timed out:\n{log}"
        );
        let timeout = if adaptive {
            DEFAULT_TIMEOUT.max(elapsed.saturating_mul(2))
        } else {
            self.timeout
        };
        if self.options.test_workspace {
            eprintln!(
                "Workspace baseline passed in {:.1}s, mutant suite timeout {:.1}s",
                elapsed.as_secs_f64(),
                timeout.as_secs_f64()
            );
        }
        let libraries_first = self.options.workspace_coverage
            && !self.options.lib
            && self.matching_libraries(&build_log)
            && self
                .library_tests(timeout)
                .is_ok_and(|s| s == Status::Survived);
        Ok(Baseline {
            artifacts,
            timeout,
            libraries_first,
        })
    }
    fn matching_targets(&self, mut cmd: Command, full: &Harnesses) -> Result<Harnesses> {
        cmd.args(["--no-run", "--message-format=json"]);
        let (status, log) = crate::runner::execute(
            cmd,
            self.temp,
            Duration::from_secs(self.options.build_timeout.get() as u64),
        )?;
        ensure!(
            status.is_some_and(|s| s.success()),
            "target-filtered build failed"
        );
        let selected = harnesses(&log)?;
        ensure!(
            selected.iter().all(|(p, t)| full.get(p) == Some(t)),
            "target filters changed a test artifact"
        );
        Ok(selected)
    }
    fn matching_libraries(&self, full_messages: &str) -> bool {
        let mut cmd = self.command();
        cmd.arg("--lib");
        harnesses(full_messages).is_ok_and(|full| self.matching_targets(cmd, &full).is_ok())
    }
    fn library_tests(&self, timeout: Duration) -> Result<Status> {
        let mut cmd = self.command();
        cmd.arg("--lib");
        Self::outcome(crate::runner::execute(cmd, self.temp, timeout)?)
    }
    fn literal(&self, id: usize) -> Result<(usize, Status)> {
        self.catalog.literal(self.root, id)?;
        let (build, msg) = self.build(self.libraries_first)?;
        let status = if build.is_none() {
            Status::Timeout
        } else if build.is_some_and(|s| s.success()) {
            // Cargo's default ordering can run long integration targets before
            // the library that detects this edit. Keep the original suite for
            // every case that the validated library subset does not kill.
            if self.libraries_first
                && self.matching_libraries(&msg)
                && self
                    .library_tests(self.timeout)
                    .is_ok_and(|s| s == Status::Killed)
            {
                eprintln!(
                    "Workspace library tests killed {}",
                    self.catalog.edits[id].name
                );
                Status::Killed
            } else {
                self.classify(None)?
            }
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
        self.completed(id, status)
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
            self.timeout,
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
    fn workspace_results(
        &self,
        ids: &[usize],
        messages: &str,
        trace: &Path,
        cargo_pid: u32,
    ) -> Result<Vec<(usize, Status)>> {
        use crate::workspace_coverage::{Scope, Target, scopes};
        let full = harnesses(messages)?;
        let groups = scopes(ids, &fs::read_to_string(trace)?, cargo_pid, &full)?;
        let mut results = Vec::new();
        let mut uncovered = 0;
        let mut filtered = 0;
        for (scope, group) in groups {
            if scope == Scope::Uncovered {
                uncovered += group.len();
                results.extend(group.into_iter().map(|id| (id, Status::Uncovered)));
                continue;
            }
            let mut selectors = BTreeSet::new();
            if let Scope::Targets(paths) = scope {
                selectors.extend(paths.iter().map(|path| full[path].clone()));
                if self.options.lib {
                    selectors.remove(&Target::Library);
                }
                if !selectors.is_empty() {
                    // Cargo can resolve a different feature set when target filters
                    // change. Only replay test binaries present in the original build.
                    let mut build = self.command();
                    for target in &selectors {
                        target.select(&mut build);
                    }
                    let same_artifacts = self
                        .matching_targets(build, &full)
                        .is_ok_and(|selected| paths.iter().all(|p| selected.contains_key(p)));
                    if !same_artifacts {
                        selectors.clear();
                    }
                }
            }
            if !selectors.is_empty() {
                filtered += group.len();
            }
            let outcomes = self.pool.install(|| {
                group
                    .par_iter()
                    .map(|&id| {
                        let mut cmd = self.command();
                        for target in &selectors {
                            target.select(&mut cmd);
                        }
                        cmd.env("CARGO_ONLINE_MUTANT", (id + 1).to_string());
                        let outcome =
                            Self::outcome(crate::runner::execute(cmd, self.temp, self.timeout)?)?;
                        Ok((id, outcome))
                    })
                    .collect::<Result<Vec<_>>>()
            })?;
            results.extend(outcomes);
        }
        eprintln!(
            "Workspace coverage: {uncovered} uncovered, {filtered} with target filters, {} with the full suite",
            ids.len() - uncovered - filtered
        );
        Ok(results)
    }
    fn mir_batch(&self, ids: &[usize], report: &mut Report) -> Result<Vec<usize>> {
        let Some(driver) = self.session_driver else {
            return Ok(ids.to_vec());
        };
        let (mut native, source): (Vec<_>, Vec<_>) = ids.iter().copied().partition(|&id| {
            !report.routes.source.contains(&id)
                && self
                    .catalog
                    .mir_site(self.root, id, &Default::default())
                    .is_some()
        });
        if native.is_empty() {
            return Ok(ids.to_vec());
        }
        let insertions = self.catalog.bundle(self.root, &source)?;
        let sites: Vec<_> = native
            .iter()
            .filter_map(|&id| self.catalog.mir_site(self.root, id, &insertions))
            .collect();
        // Keep plan names unique until the session finishes. Rustc emits the
        // name as a tracked environment dependency of each instrumented unit.
        let dir = tempfile::tempdir_in(driver.path())?.keep();
        fs::write(dir.join("sites"), sites.join("\n"))?;
        let engine = Engine {
            plan: Some(&dir),
            target: self.target.clone(),
            ..*self
        };
        let (status, log) = engine.build(true)?;
        if !status.is_some_and(|s| s.success()) {
            let suspects = self.diagnostic_variants(log.as_bytes(), &source)?;
            self.catalog.restore(self.root, &source)?;
            if !suspects.is_empty() {
                for &id in &suspects {
                    report.results.push(self.literal(id)?);
                    report.routes.literal.insert(id);
                }
                let viable: Vec<_> = ids
                    .iter()
                    .copied()
                    .filter(|i| !suspects.contains(i))
                    .collect();
                return self.mir_batch(&viable, report);
            }
            if std::env::var_os("CARGO_ONLINE_VERBOSE").is_some() {
                eprintln!("{log}");
            }
            eprintln!("MIR build failed; retrying catalog source edits");
            return Ok(ids.to_vec());
        }
        let attempted = native.clone();
        let mut matched = BTreeSet::new();
        let mut foreign = BTreeSet::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("unit-") {
                continue;
            }
            let text = fs::read_to_string(entry.path())?;
            let mut loaded = BTreeSet::new();
            let mut counts = BTreeMap::new();
            for line in text.lines() {
                if let Some(id) = line.strip_prefix("file ") {
                    loaded.insert(id.parse::<usize>()?);
                } else {
                    *counts.entry(line.parse::<usize>()?).or_insert(0usize) += 1;
                }
            }
            native.retain(|id| !loaded.contains(&(id + 1)) || counts.get(&(id + 1)) == Some(&1));
            matched.extend(counts.into_keys());
        }
        for &id in &attempted {
            if driver
                .used_by_another_unit(&self.root.join(&self.catalog.edits[id].file), self.package)?
            {
                foreign.insert(id);
            }
        }
        native.retain(|id| matched.contains(&(id + 1)) && !foreign.contains(id));
        if std::env::var_os("CARGO_ONLINE_VERBOSE").is_some() {
            eprintln!("MIR catalog indices: {native:?}");
        }
        report
            .routes
            .source
            .extend(attempted.into_iter().filter(|id| !native.contains(id)));
        let ready: BTreeSet<_> = native.iter().chain(&source).copied().collect();
        let usable = !ready.is_empty()
            && crate::mir::rebuilt_owner(&log, &self.root.join(self.package_dir))?
            && engine.run_bundle(&ready.iter().copied().collect::<Vec<_>>(), &log, report)?;
        self.catalog.restore(self.root, &source)?;
        // Another workspace target can compile the same source via #[path] or include!.
        // Rebuild literal edits there, including copies consumed by build scripts.
        for &id in &foreign {
            report.results.push(self.literal(id)?);
            report.routes.literal.insert(id);
        }
        if !usable {
            return Ok(ids
                .iter()
                .copied()
                .filter(|id| !foreign.contains(id))
                .collect());
        }
        eprintln!(
            "{} native MIR edits and {} source edits in one build",
            native.len(),
            source.len()
        );
        Ok(ids
            .iter()
            .copied()
            .filter(|id| !ready.contains(id) && !foreign.contains(id))
            .collect())
    }
    fn run_bundle(&self, ids: &[usize], log: &str, report: &mut Report) -> Result<bool> {
        let (usable, trace) = if self.options.workspace_coverage {
            let trace = tempfile::NamedTempFile::new_in(self.temp)?;
            let mut cmd = self.command();
            cmd.env("CARGO_ONLINE_TRACE", trace.path())
                .env("CARGO_ONLINE_TRACE_TARGETS", "1");
            let (pid, result) = crate::runner::execute_with_pid(cmd, self.temp, self.timeout)?;
            (
                Self::outcome(result)? == Status::Survived,
                Some((pid, trace)),
            )
        } else {
            (self.classify(None)? == Status::Survived, None)
        };
        if usable {
            let selected = if let Some((pid, trace)) = &trace {
                match self.workspace_results(ids, log, trace.path(), *pid) {
                    Ok(results) => Some(results),
                    Err(err) => {
                        eprintln!(
                            "Running the whole workspace suite after target selection failed: {err:#}"
                        );
                        None
                    }
                }
            } else if self.options.lib && !self.options.full && !self.options.test_workspace {
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
                // Selection can fall back to the full suite. Publish only
                // after the selected results have been accepted as a group.
                Some(outcomes) => outcomes
                    .into_iter()
                    .map(|(id, status)| self.completed(id, status))
                    .collect::<Result<Vec<_>>>()?,
                None => self.pool.install(|| {
                    ids.par_iter()
                        .map(|&i| self.completed(i, self.classify(Some(i))?))
                        .collect::<Result<Vec<_>>>()
                })?,
            };
            report.results.extend(results);
            report.bundled += ids.len();
            report.builds += 1;
        }
        Ok(usable)
    }
    fn batch(&self, ids: &[usize], report: &mut Report, literal: &mut Vec<usize>) -> Result<()> {
        self.catalog.bundle(self.root, ids)?;
        // A mutation may be ill-typed or valid only in its original context.
        // Prove viability with its literal edit before reporting it unviable.
        let (status, log) = self.build(true)?;
        let compiled = status.is_some_and(|s| s.success());
        let usable = compiled && self.run_bundle(ids, &log, report)?;
        let suspects = if compiled {
            BTreeSet::new()
        } else {
            self.diagnostic_variants(log.as_bytes(), ids)?
        };
        self.catalog.restore(self.root, ids)?;
        if !usable {
            if ids.len() == 1 {
                literal.push(ids[0]);
                return Ok(());
            }
            if !suspects.is_empty() {
                // These edits already triggered a compiler diagnostic. Confirm
                // them on the warm target before allocating any literal workers.
                for &id in &suspects {
                    report.results.push(self.literal(id)?);
                }
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
        routes: Routes::default(),
        results: Vec::new(),
        bundled: 0,
        builds: 0,
    };
    let compiler = options
        .mir_rustc
        .as_ref()
        .map(|c| crate::mir::Toolchain::prepare(c, &cache))
        .transpose()?;
    let session_driver = compiler
        .as_ref()
        .map(|tc| {
            let paths = catalog
                .edits
                .iter()
                .map(|edit| (root.join(&edit.file), edit.package.clone()))
                .collect();
            crate::mir::SessionDriver::new(tc, temp.path(), paths)
        })
        .transpose()?;
    let mut workspace_baseline = None;
    let route_path = {
        use std::hash::{Hash, Hasher};
        let mut hash = std::hash::DefaultHasher::new();
        (
            compiler.as_ref().map(|tc| &tc.directory),
            std::env::var_os("RUSTC"),
            &original,
            &out.stdout,
            options.lib,
            options.test_workspace,
            &options.features,
            options.all_features,
            options.no_default_features,
        )
            .hash(&mut hash);
        catalog.hash_sources(&mut hash);
        cache.join(format!("routes-{:016x}.json", hash.finish()))
    };
    if let Ok(bytes) = fs::read(&route_path) {
        report.routes = serde_json::from_slice(&bytes).unwrap_or_default();
    }
    for (package, ids) in packages {
        let manifest = meta["packages"]
            .as_array()
            .context("missing packages")?
            .iter()
            .find(|p| p["name"] == *package)
            .context("unknown mutation package")?;
        let path = crate::json_path(&manifest["manifest_path"])?;
        let package_dir = path.parent().unwrap().strip_prefix(&original)?;
        let mut engine = Engine {
            compiler: compiler.as_ref(),
            plan: None,
            session_driver: session_driver.as_ref(),
            libraries_first: false,
            timeout: options
                .timeout
                .map_or(DEFAULT_TIMEOUT, |s| Duration::from_secs(s.get() as u64)),
            options: &options,
            root: &root,
            target: cache.join("catalog-build"),
            package_dir,
            temp: temp.path(),
            catalog: &catalog,
            package,
            pool: &pool,
        };
        let package_baseline;
        let baseline = if options.test_workspace {
            if workspace_baseline.is_none() {
                workspace_baseline = Some(engine.baseline()?);
            }
            workspace_baseline.as_ref().unwrap()
        } else {
            package_baseline = engine.baseline()?;
            &package_baseline
        };
        engine.timeout = baseline.timeout;
        engine.libraries_first = baseline.libraries_first;
        let snapshot_manifest = root.join(path.strip_prefix(&original)?);
        let host_code = baseline.artifacts.as_ref().is_none_or(|build| {
            build.needs_literal(
                &snapshot_manifest,
                || engine.command(),
                || {
                    let mut cmd = cargo();
                    cmd.current_dir(&root).arg("metadata");
                    if !options.test_workspace {
                        cmd.arg("--manifest-path").arg(&snapshot_manifest);
                    }
                    options.feature_args(&mut cmd);
                    cmd
                },
            )
        });
        let (runtime, mut literal): (Vec<_>, Vec<_>) = ids.into_iter().partition(|&i| {
            catalog.bundleable(i) && !host_code && !report.routes.literal.contains(&i)
        });
        eprintln!(
            "{}: {} catalog candidates",
            package,
            runtime.len() + literal.len()
        );
        for batch in runtime.chunks(options.batch_size.get()) {
            let remaining = engine.mir_batch(batch, &mut report)?;
            if !remaining.is_empty() {
                engine.batch(&remaining, &mut report, &mut literal)?;
            }
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
    {
        // A previously ill-typed edit gets an independent compile on the next
        // run. Schedule these builds across workers and keep them out of bundles.
        report.routes.literal.extend(
            report
                .results
                .iter()
                .filter_map(|&(id, status)| (status == Status::Unviable).then_some(id)),
        );
        let tmp = tempfile::NamedTempFile::new_in(cache)?;
        serde_json::to_writer(tmp.as_file(), &report.routes)?;
        tmp.persist(route_path)?;
    }
    let mut counts = BTreeMap::new();
    for &(id, s) in &report.results {
        *counts.entry(s).or_insert(0usize) += 1;
        if !options.stream {
            println!("{s:?}\t{}", catalog.edits[id].name);
        }
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
