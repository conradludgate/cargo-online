//! Build the private driver and its runtime as one compiler-specific installation.
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
};

pub struct Toolchain {
    rustc: PathBuf,
    pub directory: PathBuf,
    sysroot: PathBuf,
    runtime: String,
    libraries: std::ffi::OsString,
}
/// The wrapper identity and source index stay fixed for one package in one session.
pub struct PackageDriver {
    directory: tempfile::TempDir,
    files: Vec<PathBuf>,
}

impl PackageDriver {
    pub fn new(compiler: &Toolchain, temp: &Path, paths: BTreeSet<PathBuf>) -> Result<Self> {
        let directory = tempfile::tempdir_in(temp)?;
        fs::hard_link(compiler.wrapper(), directory.path().join("rustc"))?;
        let files: BTreeSet<_> = paths
            .into_iter()
            .map(fs::canonicalize)
            .collect::<std::io::Result<_>>()?;
        let files: Vec<_> = files.into_iter().collect();
        let mut index = Vec::new();
        for path in &files {
            index.extend_from_slice(path.as_os_str().as_encoded_bytes());
            index.push(0);
        }
        fs::write(directory.path().join("sources"), index)?;
        fs::create_dir(directory.path().join("foreign"))?;
        Ok(Self { directory, files })
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub fn used_by_another_unit(&self, path: &Path) -> Result<bool> {
        let path = path.canonicalize()?;
        let index = self
            .files
            .binary_search(&path)
            .map_err(|_| anyhow::anyhow!("unindexed MIR source {}", path.display()))?;
        Ok(self
            .path()
            .join("foreign")
            .join(index.to_string())
            .try_exists()?)
    }
}

/// Every owned artifact must describe this plan, including feature-specific
/// builds of the same source. A Cargo cache hit has no new mapping report.
pub fn rebuilt_owner(log: &str, owner: &Path) -> Result<bool> {
    let mut rebuilt = false;
    for line in log.lines().filter(|line| line.starts_with('{')) {
        let message: serde_json::Value = serde_json::from_str(line)?;
        if message["reason"] != "compiler-artifact"
            || message["manifest_path"]
                .as_str()
                .and_then(|p| Path::new(p).parent())
                != Some(owner)
        {
            continue;
        }
        if message["fresh"].as_bool() != Some(false) {
            return Ok(false);
        }
        rebuilt = true;
    }
    Ok(rebuilt)
}

fn stdout(cmd: &mut Command) -> Result<String> {
    let out = cmd.output()?;
    ensure!(
        out.status.success(),
        "compiler command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?.trim().into())
}
impl Toolchain {
    pub fn prepare(rustc: &Path, cache: &Path) -> Result<Self> {
        let version = stdout(Command::new(rustc).arg("-vV"))?;
        let sysroot = PathBuf::from(stdout(Command::new(rustc).args(["--print", "sysroot"]))?)
            .canonicalize()?;
        // Resolve rustup proxies through the selected sysroot. Build scripts can
        // invoke rustc from a different current directory than the original CLI.
        let rustc = sysroot.join("bin/rustc");
        let mut paths = vec![sysroot.join("lib")];
        if let Some(previous) = std::env::var_os("LD_LIBRARY_PATH") {
            paths.extend(std::env::split_paths(&previous));
        }
        let libraries = std::env::join_paths(paths)?;
        let mut hash = DefaultHasher::new();
        (
            version,
            &sysroot,
            include_str!("../driver/main.rs"),
            include_str!("../driver/codegen.rs"),
            include_str!("../driver/runtime.rs"),
        )
            .hash(&mut hash);
        fs::create_dir_all(cache)?;
        let directory = cache.join(format!("driver-{:016x}", hash.finish()));
        let runtime = format!("libcargo_online_rt-{:016x}.rlib", hash.finish());
        if !directory.exists() {
            let temp = tempfile::tempdir_in(cache)?;
            let p = temp.path();
            fs::write(p.join("codegen.rs"), include_str!("../driver/codegen.rs"))?;
            for (name, source, flags, out) in [
                (
                    "runtime.rs",
                    include_str!("../driver/runtime.rs"),
                    vec!["--crate-type=rlib", "--crate-name=cargo_online_rt", "-O"],
                    runtime.as_str(),
                ),
                (
                    "driver.rs",
                    include_str!("../driver/main.rs"),
                    vec!["-C", "rpath", "-O"],
                    "cargo-online-rustc",
                ),
            ] {
                fs::write(p.join(name), source)?;
                stdout(
                    Command::new(&rustc)
                        .env("LD_LIBRARY_PATH", &libraries)
                        .arg("--edition=2024")
                        .args(flags)
                        .arg(format!("-Cmetadata={:016x}", hash.finish()))
                        .arg(format!(
                            "-Clink-arg=-Wl,-rpath,{}",
                            sysroot.join("lib").display()
                        ))
                        .arg("-L")
                        .arg(sysroot.join("lib"))
                        .arg(p.join(name))
                        .arg("-o")
                        .arg(p.join(out)),
                )?;
            }
            fs::hard_link(p.join("cargo-online-rustc"), p.join("cargo-online-rustdoc"))?;
            if let Err(e) = fs::rename(p, &directory)
                && !directory.is_dir()
            {
                return Err(e.into());
            }
        }
        Ok(Self {
            rustc,
            directory,
            sysroot,
            runtime,
            libraries,
        })
    }
    pub fn compiler(&self, cmd: &mut Command) {
        cmd.env("LD_LIBRARY_PATH", &self.libraries)
            .env("RUSTC", &self.rustc)
            .env("RUSTDOC", self.sysroot.join("bin/rustdoc"));
    }
    pub fn instrument(&self, cmd: &mut Command, wrapper: &Path, package: &Path, report: &Path) {
        self.compiler(cmd);
        cmd.env("CARGO_ONLINE_SYSROOT", &self.sysroot);
        cmd.env("RUSTDOC", self.directory.join("cargo-online-rustdoc"))
            .env("RUSTC_WORKSPACE_WRAPPER", wrapper)
            .env(
                "CARGO_ONLINE_MIR_RUNTIME",
                self.directory.join(&self.runtime),
            )
            .env("CARGO_ONLINE_MIR_REPORT", report)
            .env("CARGO_ONLINE_MIR_PACKAGE", package)
            .env_remove("CARGO_ONLINE_MIR_SOURCES")
            .env_remove("CARGO_ONLINE_INSTANCE_CODEGEN")
            .env_remove("CARGO_ONLINE_MIR_DEV")
            .env_remove("CARGO_ONLINE_MIR_PLAN");
    }
    pub fn wrapper(&self) -> PathBuf {
        self.directory.join("cargo-online-rustc")
    }
}
