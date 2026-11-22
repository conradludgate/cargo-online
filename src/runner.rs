use anyhow::{Context, Result, bail, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    io::{Read, Seek},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

pub struct Harness {
    exe: PathBuf,
    cwd: PathBuf,
    temp: PathBuf,
    timeout: Duration,
    libs: OsString,
}
pub struct Baseline<'a> {
    name: &'a str,
    pub hits: HashSet<usize>,
    pub elapsed: Duration,
}
#[derive(Clone, Copy)]
pub enum Outcome {
    Killed,
    Survived,
    Uncovered,
    Timeout,
}
impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Killed => "killed",
            Self::Survived => "survived",
            Self::Uncovered => "uncovered",
            Self::Timeout => "timed out",
        }
    }
}
impl Harness {
    pub fn new(exe: PathBuf, cwd: PathBuf, temp: &Path, timeout: Duration) -> Result<Self> {
        let mut paths = vec![exe.parent().context("missing executable directory")?.into()];
        if let Some(previous) = env::var_os("LD_LIBRARY_PATH") {
            paths.extend(env::split_paths(&previous));
        }
        Ok(Self {
            exe,
            cwd,
            temp: temp.into(),
            timeout,
            libs: env::join_paths(paths)?,
        })
    }
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.current_dir(&self.cwd)
            .env_remove("CARGO_ONLINE_TRACE")
            .env_remove("CARGO_ONLINE_MUTANT")
            .env("LD_LIBRARY_PATH", &self.libs)
            .stdin(Stdio::null())
            .process_group(0);
        cmd
    }
    pub fn tests(&self) -> Result<Vec<String>> {
        let list = |ignored| -> Result<Vec<String>> {
            let mut c = self.cmd();
            c.args(["--list", "--format", "terse"]);
            if ignored {
                c.arg("--ignored");
            }
            let (status, out) = execute(c, &self.temp, self.timeout)?;
            ensure!(
                status.is_some_and(|s| s.success()),
                "test discovery failed: {out}"
            );
            Ok(out
                .lines()
                .filter_map(|l| l.strip_suffix(": test").map(String::from))
                .collect())
        };
        let ignored: HashSet<_> = list(true)?.into_iter().collect();
        Ok(list(false)?
            .into_iter()
            .filter(|t| !ignored.contains(t))
            .collect())
    }
    fn single(&self, name: &str) -> Command {
        let mut cmd = self.cmd();
        cmd.args(["--exact", name, "--test-threads=1"]);
        cmd
    }
    pub fn baseline<'a>(&self, index: usize, name: &'a str, trace: bool) -> Result<Baseline<'a>> {
        let path = self.temp.join(format!("trace-{index}"));
        let mut cmd = self.single(name);
        if trace {
            fs::File::create(&path)?;
            cmd.env("CARGO_ONLINE_TRACE", &path);
        }
        let start = Instant::now();
        let (status, out) = execute(cmd, &self.temp, self.timeout)?;
        ensure!(
            status.is_some_and(|s| s.success()) && out.contains("1 passed;"),
            "baseline test {name} failed or timed out: {out}"
        );
        let hits = if trace {
            fs::read_to_string(path)?
                .lines()
                .map(str::parse)
                .collect::<Result<_, _>>()?
        } else {
            HashSet::new()
        };
        Ok(Baseline {
            name,
            elapsed: start.elapsed(),
            hits,
        })
    }
    pub fn classify<'a>(
        &self,
        id: usize,
        tests: &[&Baseline<'a>],
    ) -> Result<(Outcome, usize, Option<&'a str>)> {
        if tests.is_empty() {
            return Ok((Outcome::Uncovered, 0, None));
        }
        for (i, t) in tests.iter().enumerate() {
            let mut cmd = self.single(t.name);
            cmd.env("CARGO_ONLINE_MUTANT", id.to_string());
            let (status, out) = execute(cmd, &self.temp, self.timeout)?;
            match status {
                None => return Ok((Outcome::Timeout, i + 1, Some(t.name))),
                Some(s) if s.success() && out.contains("1 passed;") => {}
                Some(s) if s.code() == Some(101) || s.code().is_none() => {
                    return Ok((Outcome::Killed, i + 1, Some(t.name)));
                }
                _ => bail!("unexpected libtest result for {}: {out}", t.name),
            }
        }
        Ok((Outcome::Survived, tests.len(), None))
    }
}

pub(crate) fn execute(
    mut cmd: Command,
    temp: &Path,
    timeout: Duration,
) -> Result<(Option<ExitStatus>, String)> {
    cmd.process_group(0);
    let mut log = tempfile::tempfile_in(temp)?;
    let mut child = cmd
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .spawn()?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() >= timeout {
            break None;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    if status.is_none() {
        let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
        child.wait()?;
    }
    log.rewind()?;
    let mut buf = Vec::new();
    log.read_to_end(&mut buf)?;
    Ok((status, String::from_utf8_lossy(&buf).into_owned()))
}
