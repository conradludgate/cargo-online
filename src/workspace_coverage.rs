//! Associate function-entry traces with the harnesses Cargo built for the bundle.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Target {
    Library,
    Integration(String),
    Binary(String),
    Example(String),
    Bench(String),
}
impl Target {
    pub fn select(&self, command: &mut Command) {
        match self {
            Self::Library => command.arg("--lib"),
            Self::Integration(name) => command.arg("--test").arg(name),
            Self::Binary(name) => command.arg("--bin").arg(name),
            Self::Example(name) => command.arg("--example").arg(name),
            Self::Bench(name) => command.arg("--bench").arg(name),
        };
    }
}

pub type Harnesses = BTreeMap<String, Target>;

pub fn harnesses(messages: &str) -> Result<Harnesses> {
    let mut targets = Harnesses::new();
    let mut complete = false;
    for line in messages.lines().filter(|l| l.starts_with('{')) {
        let v: Value = serde_json::from_str(line)?;
        if v["reason"] == "build-finished" {
            complete = v["success"] == true;
        }
        if v["reason"] != "compiler-artifact" || v["profile"]["test"] != true {
            continue;
        }
        let Some(path) = v["executable"].as_str() else {
            continue;
        };
        let target = &v["target"];
        let name = target["name"].as_str().context("missing target name")?;
        let kind = target["kind"].as_array().context("missing target kinds")?;
        let selector = if kind.iter().any(|k| {
            matches!(
                k.as_str(),
                Some("lib" | "rlib" | "proc-macro" | "cdylib" | "dylib" | "staticlib")
            )
        }) {
            Target::Library
        } else {
            match kind.first().and_then(Value::as_str) {
                Some("test") => Target::Integration(name.into()),
                Some("bin") => Target::Binary(name.into()),
                Some("example") => Target::Example(name.into()),
                Some("bench") => Target::Bench(name.into()),
                _ => anyhow::bail!("unknown test target kind"),
            }
        };
        targets.insert(path.to_owned(), selector);
    }
    ensure!(
        complete && !targets.is_empty(),
        "incomplete test artifact list"
    );
    Ok(targets)
}

#[derive(Clone, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum Scope {
    #[default]
    Uncovered,
    Targets(BTreeSet<String>),
    Full,
}

pub fn scopes(
    ids: &[usize],
    trace: &str,
    cargo_pid: u32,
    harnesses: &Harnesses,
) -> Result<BTreeMap<Scope, Vec<usize>>> {
    let mut scopes: BTreeMap<_, _> = ids.iter().map(|&id| (id, Scope::Uncovered)).collect();
    ensure!(
        trace.is_empty() || trace.ends_with('\n'),
        "truncated target trace"
    );
    for record in trace.lines() {
        let (id, process) = record.split_once('\t').context("incomplete target trace")?;
        let (parent, path) = process.split_once('\t').context("missing trace parent")?;
        let id = id
            .parse::<usize>()?
            .checked_sub(1)
            .context("invalid variant id")?;
        let scope = scopes.get_mut(&id).context("unexpected variant")?;
        // A helper process can execute a known harness with different arguments.
        // Replay only harnesses launched directly by this Cargo invocation.
        if parent.parse::<u32>().ok() != Some(cargo_pid) || !harnesses.contains_key(path) {
            *scope = Scope::Full;
        } else if *scope != Scope::Full {
            if *scope == Scope::Uncovered {
                *scope = Scope::Targets(BTreeSet::new());
            }
            if let Scope::Targets(paths) = scope {
                paths.insert(path.to_owned());
            }
        }
    }
    let mut groups = BTreeMap::new();
    for (id, scope) in scopes {
        groups.entry(scope).or_insert_with(Vec::new).push(id);
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_harness_and_unidentified_processes_keep_the_full_suite() {
        let targets = Harnesses::from([("/known".into(), Target::Library)]);
        let trace = "1\t42\t/known\n1\t99\t/known\n1\t42\t/known\n2\t42\t/unknown\n";
        let groups = scopes(&[0, 1, 2], trace, 42, &targets).unwrap();
        assert_eq!(groups[&Scope::Full], [0, 1]);
        assert_eq!(groups[&Scope::Uncovered], [2]);
        assert!(scopes(&[0], "1\t42\t/known", 42, &targets).is_err());
        assert!(scopes(&[0], "4\t42\t/known\n", 42, &targets).is_err());
    }
}
