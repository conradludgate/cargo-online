//! Linked only into instrumented native test builds.
use std::collections::HashSet;
use std::io::Write;
use std::sync::{LazyLock, Mutex, OnceLock};

pub extern "C" fn active(id: u64) -> bool {
    static SELECTED: OnceLock<Option<u64>> = OnceLock::new();
    static TRACE: LazyLock<Option<std::ffi::OsString>> =
        LazyLock::new(|| std::env::var_os("CARGO_ONLINE_TRACE"));
    static SEEN: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Default::default);
    if let Some(path) = &*TRACE {
        if SEEN.lock().unwrap().insert(id) {
            let line = if std::env::var_os("CARGO_ONLINE_TRACE_TARGETS").is_some() {
                let exe = std::env::current_exe().unwrap();
                let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
                let parent = status
                    .lines()
                    .find_map(|s| s.strip_prefix("PPid:"))
                    .unwrap_or("?")
                    .trim();
                format!(
                    "{id}\t{parent}\t{}\n",
                    exe.to_str()
                        .filter(|s| !s.contains(['\t', '\r', '\n']))
                        .unwrap_or("?")
                )
            } else {
                format!("{id}\n")
            };
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
        }
    }
    *SELECTED.get_or_init(|| std::env::var("CARGO_ONLINE_MUTANT").ok()?.parse().ok()) == Some(id)
}

pub extern "C" fn enter(id: u64) {
    static PATH: LazyLock<Option<std::ffi::OsString>> =
        LazyLock::new(|| std::env::var_os("CARGO_ONLINE_DEV_TRACE"));
    thread_local! { static SEEN: std::cell::RefCell<HashSet<u64>> = Default::default(); }
    if let Some(path) = &*PATH {
        let thread = std::thread::current();
        let test = thread
            .name()
            .filter(|s| !s.contains(['\t', '\r', '\n']))
            .unwrap_or("?");
        if SEEN.with_borrow_mut(|seen| seen.insert(id)) {
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(format!("{id}\t{test}\n").as_bytes())
                .unwrap();
        }
    }
}
