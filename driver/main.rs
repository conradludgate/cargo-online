//! Cargo workspace rustc wrapper for the compiler revision documented in README.
#![feature(rustc_private)]
extern crate rustc_attr_ir;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_data_structures::{
    fingerprint::Fingerprint,
    stable_hash::{StableHash, StableHasher},
};
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::mir::*;
use rustc_middle::ty::TyCtxt;
use rustc_span::dummy_spanned;
use std::collections::HashMap;
use std::os::unix::{ffi::OsStrExt, process::CommandExt};
use std::{
    env, fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    sync::{Mutex, OnceLock},
};

mod codegen;

struct Site {
    id: u64,
    path: PathBuf,
    start: usize,
    end: usize,
    change: Change,
}
enum Change {
    Binary(BinOp, BinOp),
    Return(String),
}
struct Plan {
    sites: Vec<Site>,
    by_span: HashMap<(usize, usize), Vec<usize>>,
    dev: bool,
}
static PLAN: OnceLock<Plan> = OnceLock::new();
type Provider = for<'tcx> fn(TyCtxt<'tcx>, LocalDefId) -> &'tcx Body<'tcx>;
static ORIGINAL: OnceLock<Provider> = OnceLock::new();
static REPORT: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn op(name: &str) -> BinOp {
    match name {
        "==" => BinOp::Eq,
        "!=" => BinOp::Ne,
        "<" => BinOp::Lt,
        "<=" => BinOp::Le,
        ">" => BinOp::Gt,
        ">=" => BinOp::Ge,
        "&" => BinOp::BitAnd,
        "|" => BinOp::BitOr,
        "^" => BinOp::BitXor,
        _ => panic!("unsupported binary operator"),
    }
}
fn helper(tcx: TyCtxt<'_>, name: &str) -> DefId {
    use std::os::unix::fs::MetadataExt;
    static ACTIVE: OnceLock<DefId> = OnceLock::new();
    static ENTER: OnceLock<DefId> = OnceLock::new();
    let slot = match name {
        "active" => &ACTIVE,
        "enter" => &ENTER,
        _ => unreachable!(),
    };
    *slot.get_or_init(|| {
        // DefIds are local to this rustc invocation. Cargo can also link a
        // user dependency with the same crate name but a different signature.
        let expected = fs::metadata(env::var_os("CARGO_ONLINE_MIR_RUNTIME").unwrap()).unwrap();
        let krate = tcx
            .crates(())
            .iter()
            .find(|&&k| {
                tcx.crate_name(k).as_str() == "cargo_online_rt"
                    && tcx
                        .used_crate_source(k)
                        .rlib
                        .as_ref()
                        .and_then(|p| fs::metadata(p).ok())
                        .is_some_and(|m| m.dev() == expected.dev() && m.ino() == expected.ino())
            })
            .expect("missing instrumentation runtime archive");
        tcx.module_children(krate.as_def_id())
            .iter()
            .find(|item| item.ident.name.as_str() == name)
            .unwrap()
            .res
            .def_id()
    })
}
fn call<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: DefId,
    id: u64,
    dest: Local,
    next: BasicBlock,
    info: SourceInfo,
) -> Terminator<'tcx> {
    let arg = Operand::const_from_scalar(tcx, tcx.types.u64, Scalar::from_u64(id), info.span);
    Terminator {
        source_info: info,
        kind: TerminatorKind::Call {
            func: Operand::function_handle(tcx, def, &[], info.span),
            args: [dummy_spanned(arg)].into(),
            destination: dest.into(),
            target: Some(next),
            // Both runtime entry points use the non-unwinding C ABI.
            unwind: UnwindAction::Unreachable,
            call_source: CallSource::Misc,
            fn_span: info.span,
        },
    }
}
fn runtime_body(tcx: TyCtxt<'_>, def: LocalDefId, closures: bool) -> bool {
    let mut owner = def.to_def_id();
    while closures && tcx.def_kind(owner) == DefKind::Closure {
        if tcx.is_const_fn(owner)
            || tcx.coroutine_kind(owner).is_some_and(|kind| {
                !matches!(
                    kind,
                    rustc_hir::CoroutineKind::Desugared(rustc_hir::CoroutineDesugaring::Async, _)
                )
            })
        {
            return false;
        }
        owner = tcx.parent(owner);
    }
    matches!(tcx.def_kind(owner), DefKind::Fn | DefKind::AssocFn)
        && !tcx.is_const_fn(owner)
        && !tcx
            .codegen_fn_attrs(owner)
            .flags
            .contains(CodegenFnAttrFlags::NAKED)
}
fn transform(tcx: TyCtxt<'_>, def: LocalDefId) -> &Body<'_> {
    let body = ORIGINAL.get().unwrap()(tcx, def);
    let plan = PLAN.get().unwrap();
    if !runtime_body(tcx, def, !plan.dev) {
        return body;
    }
    if plan.dev {
        let mut h = DefaultHasher::new();
        tcx.def_path_hash(def.to_def_id()).hash(&mut h);
        let id = h.finish();
        let mut b = body.clone();
        let info = SourceInfo::outermost(body.span);
        let dest = b
            .local_decls
            .push(LocalDecl::new(tcx.types.unit, body.span));
        let blocks = b.basic_blocks_mut();
        let orig = std::mem::replace(&mut blocks[START_BLOCK], BasicBlockData::new(None, false));
        // Preserve backedges to the original entry by using a side-effect-only probe.
        let next = blocks.push(orig);
        blocks[START_BLOCK].terminator =
            Some(call(tcx, helper(tcx, "enter"), id, dest, next, info));
        return tcx.arena.alloc(b);
    }
    let mut edits = Vec::new();
    let sm = tcx.sess.source_map();
    for (bb, block) in body.basic_blocks.iter_enumerated() {
        if block.is_cleanup {
            continue;
        }
        for (index, stmt) in block.statements.iter().enumerate().rev() {
            let StatementKind::Assign(a) = &stmt.kind else {
                continue;
            };
            let Rvalue::BinaryOp(bin, operands) = &a.1 else {
                continue;
            };
            let span = stmt.source_info.span;
            if span.is_dummy() || span.from_expansion() {
                continue;
            }
            if !operands.0.ty(&body.local_decls, tcx).is_primitive() {
                continue;
            }
            let start = sm.lookup_byte_offset(span.lo());
            let end = sm.lookup_byte_offset(span.hi());
            let Some(indices) = plan.by_span.get(&(
                start.sf.original_relative_byte_pos(span.lo()).0 as usize,
                end.sf.original_relative_byte_pos(span.hi()).0 as usize,
            )) else {
                continue;
            };
            let path = PathBuf::from(start.sf.name.prefer_local_unconditionally().to_string())
                .canonicalize()
                .ok();
            let mut matches = Vec::new();
            for &i in indices {
                let site = &plan.sites[i];
                if matches!(site.change,Change::Binary(from,_) if *bin==from)
                    && path.as_ref() == Some(&site.path)
                {
                    matches.push(site);
                }
            }
            if !matches.is_empty() {
                edits.push((bb, index, matches));
            }
        }
    }
    let span = tcx.hir_body_owned_by(def).value.span;
    let start = sm.lookup_byte_offset(span.lo());
    let end = sm.lookup_byte_offset(span.hi());
    let file = Path::new(&start.sf.name.prefer_local_unconditionally().to_string())
        .canonicalize()
        .ok();
    let mut returns = Vec::new();
    if tcx.def_kind(def) != DefKind::Closure
        && !span.from_expansion()
        && body
            .args_iter()
            .all(|a| !body.local_decls[a].ty.needs_drop(tcx, body.typing_env(tcx)))
    {
        let ty = body.return_ty();
        for &i in plan
            .by_span
            .get(&(
                start.sf.original_relative_byte_pos(span.lo()).0 as usize,
                end.sf.original_relative_byte_pos(span.hi()).0 as usize,
            ))
            .into_iter()
            .flatten()
        {
            let site = &plan.sites[i];
            let Change::Return(value) = &site.change else {
                continue;
            };
            if file.as_ref() != Some(&site.path)
                || start.sf.original_relative_byte_pos(span.lo()).0 as usize != site.start
                || end.sf.original_relative_byte_pos(span.hi()).0 as usize != site.end
            {
                continue;
            }
            let literal = match ty.kind() {
                rustc_middle::ty::Bool => value.parse().ok().map(Scalar::from_bool),
                rustc_middle::ty::Int(_) => value.parse::<i128>().ok().and_then(|n| {
                    tcx.layout_of(body.typing_env(tcx).as_query_input(ty))
                        .ok()
                        .map(|l| Scalar::from_int(n, l.size))
                }),
                rustc_middle::ty::Uint(_) => value.parse::<u128>().ok().and_then(|n| {
                    tcx.layout_of(body.typing_env(tcx).as_query_input(ty))
                        .ok()
                        .map(|l| Scalar::from_uint(n, l.size))
                }),
                _ => None,
            };
            if let Some(v) = literal {
                returns.push((site.id, v));
            }
        }
    }
    if edits.is_empty() && returns.is_empty() {
        return body;
    }
    let mut b = body.clone();
    for (bb, index, sites) in edits {
        let info = b.basic_blocks[bb].statements[index].source_info;
        let dest = b
            .local_decls
            .push(LocalDecl::new(tcx.types.bool, info.span));
        let blocks = b.basic_blocks_mut();
        let mut tail = BasicBlockData::new(blocks[bb].terminator.take(), false);
        tail.statements = blocks[bb].statements.split_off(index + 1);
        let cont = blocks.push(tail);
        let original = blocks[bb].statements.pop().unwrap();
        let goto = Terminator {
            source_info: info,
            kind: TerminatorKind::Goto {
                target: cont,
                loop_hint_attrs: Default::default(),
            },
        };
        let mut normal = BasicBlockData::new(Some(goto), false);
        normal.statements.push(original);
        let mut next = blocks.push(normal.clone());
        for site in sites {
            let mut alternate = normal.clone();
            let StatementKind::Assign(a) = &mut alternate.statements[0].kind else {
                unreachable!()
            };
            let Rvalue::BinaryOp(bin, _) = &mut a.1 else {
                unreachable!()
            };
            let Change::Binary(_, to) = site.change else {
                unreachable!()
            };
            *bin = to;
            let yes = blocks.push(alternate);
            let branch = blocks.push(BasicBlockData::new(
                Some(Terminator {
                    source_info: info,
                    kind: TerminatorKind::SwitchInt {
                        discr: Operand::Copy(dest.into()),
                        targets: SwitchTargets::static_if(0, next, yes),
                    },
                }),
                false,
            ));
            next = blocks.push(BasicBlockData::new(
                Some(call(
                    tcx,
                    helper(tcx, "active"),
                    site.id,
                    dest,
                    branch,
                    info,
                )),
                false,
            ));
            REPORT.lock().unwrap().push(site.id.to_string());
        }
        blocks[bb].terminator = Some(Terminator {
            source_info: info,
            kind: TerminatorKind::Goto {
                target: next,
                loop_hint_attrs: Default::default(),
            },
        });
    }
    for (id, literal) in returns {
        let info = SourceInfo::outermost(span);
        let value = Operand::const_from_scalar(tcx, b.return_ty(), literal, span);
        let dest = b.local_decls.push(LocalDecl::new(tcx.types.bool, span));
        let blocks = b.basic_blocks_mut();
        let original =
            std::mem::replace(&mut blocks[START_BLOCK], BasicBlockData::new(None, false));
        let no = blocks.push(original);
        let mut ret = BasicBlockData::new(
            Some(Terminator {
                source_info: info,
                kind: TerminatorKind::Return,
            }),
            false,
        );
        ret.statements.push(Statement::new(
            info,
            StatementKind::Assign(Box::new((
                RETURN_PLACE.into(),
                Rvalue::Use(value, WithRetag::No),
            ))),
        ));
        let yes = blocks.push(ret);
        let branch = blocks.push(BasicBlockData::new(
            Some(Terminator {
                source_info: info,
                kind: TerminatorKind::SwitchInt {
                    discr: Operand::Copy(dest.into()),
                    targets: SwitchTargets::static_if(0, no, yes),
                },
            }),
            false,
        ));
        blocks[START_BLOCK].terminator =
            Some(call(tcx, helper(tcx, "active"), id, dest, branch, info));
        REPORT.lock().unwrap().push(id.to_string());
    }
    tcx.arena.alloc(b)
}
struct Callbacks {
    instrument: bool,
}
impl rustc_driver::Callbacks for Callbacks {
    fn after_analysis<'tcx>(
        &mut self,
        _: &rustc_interface::interface::Compiler,
        tcx: TyCtxt<'tcx>,
    ) -> rustc_driver::Compilation {
        if PLAN.get().unwrap().dev {
            let sm = tcx.sess.source_map();
            for def in tcx.hir_body_owners() {
                if !matches!(tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn) {
                    continue;
                }
                let mut h = DefaultHasher::new();
                tcx.def_path_hash(def.to_def_id()).hash(&mut h);
                let id = h.finish();
                let mut h = DefaultHasher::new();
                if !tcx.is_const_fn(def.to_def_id()) {
                    let body = tcx.optimized_mir(def);
                    let fingerprint: Fingerprint = tcx.with_stable_hashing_context(|mut cx| {
                        let mut hash = StableHasher::new();
                        cx.while_hashing_spans(false, |cx| body.stable_hash(cx, &mut hash));
                        hash.finish()
                    });
                    fingerprint.hash(&mut h);
                }
                sm.span_to_snippet(tcx.hir_span_with_body(tcx.local_def_id_to_hir_id(def)))
                    .unwrap_or_default()
                    .hash(&mut h);
                REPORT.lock().unwrap().push(format!("{id}\t{}", h.finish()));
            }
        }
        if !PLAN.get().unwrap().dev {
            let sm = tcx.sess.source_map();
            let files: std::collections::BTreeSet<_> = sm
                .files()
                .iter()
                .filter(|sf| !sf.is_imported())
                .filter_map(|sf| {
                    Path::new(&sf.name.prefer_local_unconditionally().to_string())
                        .canonicalize()
                        .ok()
                })
                .collect();
            if !self.instrument {
                // Cargo may reuse this foreign unit for many subsequent plans.
                // Record every candidate source it loaded, including sites
                // that only appear in a later batch.
                let index = PathBuf::from(env::var_os("CARGO_ONLINE_MIR_SOURCE_INDEX").unwrap());
                let flags = index.parent().unwrap().join("foreign");
                for (n, bytes) in fs::read(index).unwrap().split(|&b| b == 0).enumerate() {
                    if files.contains(Path::new(std::ffi::OsStr::from_bytes(bytes))) {
                        fs::write(flags.join(n.to_string()), []).unwrap();
                    }
                }
            }
            if self.instrument {
                for def in tcx.hir_body_owners() {
                    if !runtime_body(tcx, def, true) {
                        continue;
                    }
                    let path = sm
                        .span_to_filename(tcx.def_span(def))
                        .prefer_local_unconditionally()
                        .to_string();
                    if let Ok(path) = Path::new(&path).canonicalize()
                        && PLAN.get().unwrap().sites.iter().any(|s| s.path == path)
                    {
                        let _ = tcx.optimized_mir(def);
                    }
                }
            }
            for site in &PLAN.get().unwrap().sites {
                if files.contains(&site.path) {
                    REPORT.lock().unwrap().push(format!("file {}", site.id));
                }
            }
        }
        rustc_driver::Compilation::Continue
    }
    fn config(&mut self, c: &mut rustc_interface::Config) {
        if !self.instrument {
            return;
        }
        if !PLAN.get().unwrap().dev {
            c.track_state = Some(Box::new(|sess| {
                let name = "CARGO_ONLINE_MIR_PLAN";
                let plan = env::var(name).unwrap();
                sess.env_depinfo.borrow_mut().insert((
                    rustc_span::Symbol::intern(name),
                    Some(rustc_span::Symbol::intern(&plan)),
                ));
            }));
        }
        c.override_queries = Some(|_, p| {
            ORIGINAL.set(p.queries.optimized_mir).unwrap();
            if env::var_os("CARGO_ONLINE_INSTANCE_CODEGEN").is_some() {
                codegen::ORIGINAL
                    .set(p.queries.collect_and_partition_mono_items)
                    .unwrap();
                p.queries.collect_and_partition_mono_items = codegen::partition;
            }
            p.queries.optimized_mir = transform;
        });
    }
}
fn main() -> ExitCode {
    if env::args_os()
        .next()
        .as_deref()
        .and_then(|p| Path::new(p).file_name())
        == Some(std::ffi::OsStr::new("cargo-online-rustdoc"))
    {
        let sysroot = PathBuf::from(env::var_os("CARGO_ONLINE_SYSROOT").unwrap());
        let runtime = PathBuf::from(env::var_os("CARGO_ONLINE_MIR_RUNTIME").unwrap());
        let mut search = std::ffi::OsString::from("dependency=");
        search.push(runtime.parent().unwrap());
        let error = Command::new(sysroot.join("bin/rustdoc"))
            .args(env::args_os().skip(1))
            .arg("-L")
            .arg(search)
            .exec();
        panic!("{error}");
    }
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    let dev = env::var_os("CARGO_ONLINE_MIR_DEV").is_some();
    let owner = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from)
        == env::var_os("CARGO_ONLINE_MIR_PACKAGE").map(PathBuf::from)
        && env::var_os("CARGO_ONLINE_MIR_PACKAGE").is_some()
        && args.iter().any(|s| s == "--out-dir")
        && (!dev || args.iter().any(|s| s == "--test"))
        && !args
            .windows(2)
            .any(|a| a[0] == "--crate-name" && a[1] == "build_script_build");
    let selected = if dev {
        args.iter().any(|a| a == "--test")
            && env::var_os("CARGO_ONLINE_MIR_SOURCES").is_some_and(|sources| {
                std::env::split_paths(&sources).any(|p| {
                    args.iter().any(|a| {
                        a.ends_with(".rs")
                            && Path::new(a)
                                .canonicalize()
                                .is_ok_and(|a| p.canonicalize().as_ref().ok() == Some(&a))
                    })
                })
            })
    } else {
        owner
    };
    if !args.iter().any(|s| s == "--out-dir") || (dev && !selected) {
        let e = Command::new(&args[0]).args(&args[1..]).exec();
        panic!("{e}")
    }
    let sites: Vec<Site> = env::var_os("CARGO_ONLINE_MIR_PLAN")
        .map(|p| {
            fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|line| {
                    let f: Vec<_> = line.split('\t').collect();
                    Site {
                        id: f[0].parse().unwrap(),
                        start: f[1].parse().unwrap(),
                        end: f[2].parse().unwrap(),
                        change: if f[3] == "return" {
                            Change::Return(f[4].to_string())
                        } else {
                            Change::Binary(op(f[3]), op(f[4]))
                        },
                        path: f[5].into(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let mut by_span = HashMap::<_, Vec<_>>::new();
    for (i, site) in sites.iter().enumerate() {
        by_span.entry((site.start, site.end)).or_default().push(i);
    }
    PLAN.set(Plan {
        dev,
        sites,
        by_span,
    })
    .ok()
    .unwrap();
    let runtime = PathBuf::from(env::var_os("CARGO_ONLINE_MIR_RUNTIME").unwrap());
    // Cargo may give feature-specific units different dependency directories.
    args.push(format!(
        "-Ldependency={}",
        runtime.parent().unwrap().display()
    ));
    if selected {
        let out_dir = Path::new(&args[args.iter().position(|a| a == "--out-dir").unwrap() + 1]);
        // Subsequent Cargo/rustdoc invocations find transitive rlibs in this directory.
        let dependency = out_dir.join(runtime.file_name().unwrap());
        match fs::hard_link(&runtime, &dependency) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("link runtime into dependency search path: {e}"),
        }
        args.extend([
            "--sysroot".into(),
            env::var("CARGO_ONLINE_SYSROOT").unwrap(),
            "--extern".into(),
            format!(
                "force:cargo_online_rt={}",
                env::var("CARGO_ONLINE_MIR_RUNTIME").unwrap()
            ),
            "-Zunstable-options".into(),
            "-Zmir-opt-level=0".into(),
            "-Zinline-mir=no".into(),
        ]);
    } else {
        args.extend([
            "--sysroot".into(),
            env::var("CARGO_ONLINE_SYSROOT").unwrap(),
        ]);
    }
    let status = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::compiler_entrypoint(
            &args,
            &mut Callbacks {
                instrument: selected,
            },
        )
    });
    if status == ExitCode::SUCCESS && (dev || selected) {
        let out = env::var_os("CARGO_ONLINE_MIR_REPORT").unwrap();
        let mut out = PathBuf::from(out);
        let name = args.windows(2).find(|v| v[0] == "--crate-name").unwrap()[1].as_str();
        let extra = args
            .iter()
            .find_map(|a| a.strip_prefix("extra-filename="))
            .unwrap_or("");
        let prefix = if dev { "bodies" } else { "unit" };
        out.set_file_name(format!("{prefix}-{name}{extra}"));
        fs::write(out, REPORT.lock().unwrap().join("\n")).unwrap();
    }
    status
}
