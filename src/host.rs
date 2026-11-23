//! Cargo's clean-build artifacts scope the resolved graph to the selected suite.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
struct Artifact {
    package_id: String,
    manifest_path: PathBuf,
    target: Target,
    profile: Profile,
    features: Vec<String>,
}
#[derive(Deserialize)]
struct Target {
    src_path: PathBuf,
    kind: Vec<String>,
}
#[derive(Deserialize)]
struct Profile {
    test: bool,
}

type UnitSignature = (String, PathBuf, BTreeSet<String>, bool);

#[derive(Default)]
pub struct Build {
    ids: BTreeMap<PathBuf, String>,
    inputs: BTreeSet<String>,
    scripts: BTreeSet<String>,
    macros: BTreeSet<String>,
    features: BTreeMap<String, BTreeSet<String>>,
    artifacts: BTreeSet<UnitSignature>,
    host_units: OnceCell<Option<BTreeSet<String>>>,
    graph: OnceCell<Option<BTreeMap<String, Node>>>,
}

impl Build {
    pub fn from_messages(log: &str) -> Result<Self> {
        let mut build = Self::default();
        let mut finished = false;
        for line in log.lines().filter(|l| l.starts_with('{')) {
            let value: serde_json::Value = serde_json::from_str(line)?;
            match value["reason"].as_str() {
                Some("build-finished") => finished = value["success"] == true,
                Some("compiler-artifact") => {
                    let a: Artifact = serde_json::from_value(value)?;
                    let id = a.package_id;
                    build.artifacts.insert((
                        id.clone(),
                        a.target.src_path,
                        a.features.iter().cloned().collect(),
                        a.profile.test,
                    ));
                    build.ids.insert(a.manifest_path, id.clone());
                    build
                        .features
                        .entry(id.clone())
                        .or_default()
                        .extend(a.features);
                    for kind in &a.target.kind {
                        match kind.as_str() {
                            "custom-build" => {
                                build.scripts.insert(id.clone());
                            }
                            "proc-macro" => {
                                build.macros.insert(id.clone());
                            }
                            _ if !a.profile.test => {
                                build.inputs.insert(id.clone());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        ensure!(finished, "missing Cargo build-finished message");
        Ok(build)
    }

    pub fn needs_literal(
        &self,
        manifest: &Path,
        suite: impl FnOnce() -> Command,
        metadata: impl FnOnce() -> Command,
    ) -> bool {
        let Some(id) = self.ids.get(manifest) else {
            return true;
        };
        if self.scripts.contains(id) || self.macros.contains(id) {
            return true;
        }
        // A test harness cannot supply a build dependency. Fresh artifacts
        // appear in Cargo's messages just like newly compiled artifacts.
        if !self.inputs.contains(id) || (self.scripts.is_empty() && self.macros.is_empty()) {
            return false;
        }
        if let Some(host) = self.host_units.get_or_init(|| {
            self.read_host_units(suite())
                .inspect_err(|e| {
                    if std::env::var_os("CARGO_ONLINE_VERBOSE").is_some() {
                        eprintln!("Cargo unit graph unavailable: {e:#}");
                    }
                })
                .ok()
        }) {
            return host.contains(id);
        }
        let graph = self.graph.get_or_init(|| {
            // Metadata is optional evidence. Incomplete offline metadata must
            // never relax the decision or trigger a dependency download.
            read_graph(metadata()).ok()
        });
        graph
            .as_ref()
            .and_then(|g| self.host_uses(id, g))
            .unwrap_or(true)
    }

    fn read_host_units(&self, mut command: Command) -> Result<BTreeSet<String>> {
        #[derive(Deserialize)]
        struct Plan {
            version: u32,
            units: Vec<Unit>,
            roots: Vec<usize>,
        }
        #[derive(Deserialize)]
        struct Unit {
            pkg_id: String,
            target: Target,
            mode: String,
            features: BTreeSet<String>,
            dependencies: Vec<Dependency>,
        }
        #[derive(Deserialize)]
        struct Dependency {
            index: usize,
        }

        let output = command
            .args(["--offline", "-Z", "unstable-options", "--unit-graph"])
            .output()?;
        ensure!(output.status.success(), "Cargo unit graph unavailable");
        let plan: Plan = serde_json::from_slice(&output.stdout)?;
        ensure!(
            plan.version == 1
                && !plan.roots.is_empty()
                && plan.roots.iter().all(|&i| i < plan.units.len()),
            "unsupported Cargo unit graph"
        );
        let mut units = BTreeSet::new();
        let mut pending = Vec::new();
        for (i, u) in plan.units.iter().enumerate() {
            ensure!(
                u.dependencies.iter().all(|d| d.index < plan.units.len()),
                "invalid dependency index"
            );
            units.insert((
                u.pkg_id.clone(),
                u.target.src_path.clone(),
                u.features.clone(),
                u.mode == "test",
            ));
            if u.target
                .kind
                .iter()
                .any(|k| k == "custom-build" || k == "proc-macro")
            {
                pending.push(i);
            }
        }
        // Match actual compiler artifacts, including feature resolution, before
        // using the finer graph to relax any package-level fallback.
        ensure!(
            self.artifacts.is_subset(&units),
            "unit graph disagrees with clean build: {:?}",
            self.artifacts.difference(&units).next()
        );
        let mut visited = BTreeSet::new();
        while let Some(i) = pending.pop() {
            if visited.insert(i) {
                pending.extend(plan.units[i].dependencies.iter().map(|d| d.index));
            }
        }
        Ok(visited
            .into_iter()
            .map(|i| plan.units[i].pkg_id.clone())
            .collect())
    }
    fn host_uses(&self, id: &str, graph: &BTreeMap<String, Node>) -> Option<bool> {
        let mut pending: Vec<_> = self
            .scripts
            .iter()
            .map(|s| (s.as_str(), Some("build")))
            .chain(self.macros.iter().map(|s| (s.as_str(), None)))
            .collect();
        let mut seen = BTreeSet::new();
        while let Some((parent, kind)) = pending.pop() {
            if !seen.insert((parent, kind)) {
                continue;
            }
            let node = graph.get(parent)?;
            if !self.features.get(parent)?.is_subset(&node.features) {
                return None;
            }
            for dep in &node.deps {
                if self.inputs.contains(&dep.pkg)
                    && dep.dep_kinds.iter().any(|k| k.kind.as_deref() == kind)
                {
                    if dep.pkg == id {
                        return Some(true);
                    }
                    pending.push((&dep.pkg, None));
                }
            }
        }
        Some(false)
    }
}

#[derive(Deserialize)]
struct Metadata {
    resolve: Resolve,
}
#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}
#[derive(Deserialize)]
struct Node {
    id: String,
    features: BTreeSet<String>,
    deps: Vec<Edge>,
}
#[derive(Deserialize)]
struct Edge {
    pkg: String,
    dep_kinds: Vec<Kind>,
}
#[derive(Deserialize)]
struct Kind {
    kind: Option<String>,
}

fn read_graph(mut command: Command) -> Result<BTreeMap<String, Node>> {
    let out = command.args(["--format-version=1", "--offline"]).output()?;
    ensure!(
        out.status.success(),
        "offline dependency metadata is unavailable"
    );
    let meta: Metadata = serde_json::from_slice(&out.stdout).context("missing dependency graph")?;
    Ok(meta
        .resolve
        .nodes
        .into_iter()
        .map(|n| (n.id.clone(), n))
        .collect())
}
