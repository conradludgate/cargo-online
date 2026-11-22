#![feature(rustc_private)]

//! Standalone experiment for one cargo-mutants primitive `==` to `!=` edit.
//! Tested with rustc a267dc9278932a9a47afdd87033b6ab9c3c4c4f5 and its matching
//! rustc-dev libraries. Build with the matching rustc and `-L <sysroot>/lib -C rpath`.
//! Invoke as a rustc replacement for a single test target, with `--sysroot <sysroot>`,
//! `CARGO_INCREMENTAL=0` and `MIR_EQ_SITE=relative/source.rs:line:column`.
//! Use a disposable output directory and consume the artifact only on exit code zero.

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;

use rustc_hir::def_id::LocalDefId;
use rustc_middle::mir::{BinOp, Body, Rvalue, StatementKind};
use rustc_middle::ty::TyCtxt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

type Provider = for<'tcx> fn(TyCtxt<'tcx>, LocalDefId) -> &'tcx Body<'tcx>;
static ORIGINAL: OnceLock<Provider> = OnceLock::new();
static SITE: OnceLock<(String, usize, usize)> = OnceLock::new();
static MATCHES: AtomicUsize = AtomicUsize::new(0);

fn replace_eq(tcx: TyCtxt<'_>, def: LocalDefId) -> &Body<'_> {
    let body = ORIGINAL.get().unwrap()(tcx, def);
    let (file, line, column) = SITE.get().unwrap();
    let sm = tcx.sess.source_map();
    let defspan = tcx.def_span(def);
    if !Path::new(
        &sm.span_to_filename(defspan)
            .prefer_local_unconditionally()
            .to_string(),
    )
    .ends_with(file)
    {
        return body;
    }
    let mut changed = body.clone();
    for block in changed.basic_blocks_mut() {
        for stmt in &mut block.statements {
            if let StatementKind::Assign(assign) = &mut stmt.kind
                && let Rvalue::BinaryOp(op @ BinOp::Eq, _) = &mut assign.1
            {
                let span = stmt.source_info.span;
                if span.from_expansion() || span.is_dummy() {
                    continue;
                }
                let start = sm.lookup_char_pos(span.lo());
                let end = sm.lookup_char_pos(span.hi());
                if start.line != *line
                    || end.line != *line
                    || start.col.0 + 1 > *column
                    || end.col.0.saturating_sub(1) < *column
                    || !Path::new(&start.file.name.prefer_local_unconditionally().to_string())
                        .ends_with(file)
                {
                    continue;
                }
                *op = BinOp::Ne;
                MATCHES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    tcx.arena.alloc(changed)
}
struct Callbacks;
impl rustc_driver::Callbacks for Callbacks {
    fn config(&mut self, cfg: &mut rustc_interface::Config) {
        cfg.override_queries = Some(|_, p| {
            ORIGINAL.set(p.queries.optimized_mir).unwrap();
            p.queries.optimized_mir = replace_eq;
        });
    }
}
fn main() -> std::process::ExitCode {
    let site = std::env::var("MIR_EQ_SITE").expect("file:line:column");
    let mut parts = site.rsplitn(3, ':');
    let col = parts.next().unwrap().parse().unwrap();
    let line = parts.next().unwrap().parse().unwrap();
    SITE.set((parts.next().unwrap().to_string(), line, col))
        .unwrap();
    let (file, line, column) = SITE.get().unwrap();
    let is_equality = std::fs::read_to_string(file).ok().is_some_and(|text| {
        line.checked_sub(1)
            .and_then(|row| text.lines().nth(row))
            .is_some_and(|s| *column > 0 && s.chars().skip(column - 1).take(2).eq("==".chars()))
    });
    let args: Vec<_> = std::env::args().collect();
    if !is_equality
        || args.iter().any(|a| {
            a.trim_start_matches("-C")
                .trim_start()
                .starts_with("incremental=")
        })
    {
        eprintln!("expected a source equality and incremental compilation disabled");
        return std::process::ExitCode::FAILURE;
    }
    let status = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::compiler_entrypoint(&args, &mut Callbacks)
    });
    if status == std::process::ExitCode::SUCCESS && MATCHES.load(Ordering::Relaxed) != 1 {
        eprintln!("expected exactly one MIR equality at the selected source position");
        return std::process::ExitCode::FAILURE;
    }
    status
}
