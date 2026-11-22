use anyhow::{Context, Result};
use proc_macro2::Span;
use std::{
    fs,
    path::{Path, PathBuf},
};
use syn::{
    BinOp, Expr, Item,
    spanned::Spanned,
    visit::{self, Visit},
};

pub struct Site {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Default)]
struct Scan {
    spans: Vec<Span>,
    mod_dir: PathBuf,
    modules: Vec<PathBuf>,
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg") && a.parse_args::<syn::Ident>().is_ok_and(|i| i == "test"))
    })
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_item(&mut self, item: &'ast Item) {
        match item {
            Item::Fn(f) if f.sig.constness.is_none() && !test_only(&f.attrs) => {
                self.visit_block(&f.block)
            }
            Item::Mod(m)
                if !test_only(&m.attrs) && !m.attrs.iter().any(|a| a.path().is_ident("path")) =>
            {
                let name = m.ident.to_string();
                let dir = self.mod_dir.join(name.trim_start_matches("r#"));
                if m.content.is_some() {
                    let parent = std::mem::replace(&mut self.mod_dir, dir);
                    visit::visit_item_mod(self, m);
                    self.mod_dir = parent;
                } else {
                    self.modules.push(dir);
                }
            }
            Item::Impl(i) if !test_only(&i.attrs) => visit::visit_item_impl(self, i),
            Item::Trait(t) if !test_only(&t.attrs) => visit::visit_item_trait(self, t),
            _ => {}
        }
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if let syn::ImplItem::Fn(f) = item
            && f.sig.constness.is_none()
            && !test_only(&f.attrs)
        {
            self.visit_block(&f.block);
        }
    }
    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if let syn::TraitItem::Fn(f) = item
            && !test_only(&f.attrs)
            && let Some(b) = &f.default
        {
            self.visit_block(b);
        }
    }
    fn visit_type(&mut self, _: &'ast syn::Type) {}
    fn visit_pat(&mut self, _: &'ast syn::Pat) {}
    fn visit_generic_argument(&mut self, _: &'ast syn::GenericArgument) {}
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
    fn visit_expr(&mut self, e: &'ast Expr) {
        match e {
            Expr::Binary(b)
                if matches!(
                    b.op,
                    BinOp::Eq(_)
                        | BinOp::Ne(_)
                        | BinOp::Lt(_)
                        | BinOp::Le(_)
                        | BinOp::Gt(_)
                        | BinOp::Ge(_)
                ) =>
            {
                self.spans.push(e.span())
            }
            Expr::Const(_) => return,
            Expr::Repeat(r) => return self.visit_expr(&r.expr),
            _ => {}
        }
        visit::visit_expr(self, e);
    }
}

pub fn instrument(root: &Path, src: &Path, mod_dir: &Path, sites: &mut Vec<Site>) -> Result<()> {
    let path = root.join(src);
    let source = fs::read_to_string(&path)?;
    let syntax = syn::parse_file(&source).with_context(|| format!("parsing {}", src.display()))?;
    let prefix = source.strip_prefix('\u{feff}').map_or(0, |_| 3)
        + syntax.shebang.as_ref().map_or(0, String::len);
    let mut scan = Scan {
        mod_dir: mod_dir.into(),
        ..Scan::default()
    };
    scan.visit_file(&syntax);
    let mut edits = Vec::new();
    for span in scan.spans {
        let range = span.byte_range();
        sites.push(Site {
            file: src.into(),
            line: span.start().line,
            column: span.start().column + 1,
        });
        edits.push((prefix + range.start, "((".to_owned()));
        edits.push((
            prefix + range.end,
            format!(") ^ crate::__cargo_online::active({}))", sites.len()),
        ));
    }

    if !edits.is_empty() {
        edits.sort_by_key(|e| e.0);
        let mut rewritten = String::new();
        let mut previous = 0;
        for (offset, insertion) in edits {
            rewritten.push_str(&source[previous..offset]);
            rewritten.push_str(&insertion);
            previous = offset;
        }
        rewritten.push_str(&source[previous..]);
        fs::write(path, rewritten)?;
    }
    for dir in scan.modules {
        for file in [dir.with_extension("rs"), dir.join("mod.rs")] {
            if root.join(&file).is_file() {
                instrument(root, &file, &dir, sites)?;
                break;
            }
        }
    }
    Ok(())
}

pub const RUNTIME: &str = r#"
#[doc(hidden)]
mod __cargo_online {
    extern crate std;
    use self::std::{env, fs::OpenOptions, io::Write, sync::{Mutex, OnceLock, atomic::{AtomicBool, Ordering}}};
    static MODE: OnceLock<(usize, Option<Mutex<std::fs::File>>)> = OnceLock::new();
    static SEEN: [AtomicBool; SITE_COUNT] = [const { AtomicBool::new(false) }; SITE_COUNT];
    pub(crate) fn active(id: usize) -> bool {
        let (selected, trace) = MODE.get_or_init(|| {
            let selected = env::var("CARGO_ONLINE_MUTANT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            let trace = env::var_os("CARGO_ONLINE_TRACE").map(|p| Mutex::new(OpenOptions::new().append(true).open(p).unwrap()));
            (selected, trace)
        });
        if let Some(trace) = trace {
            if !SEEN[id].load(Ordering::Relaxed) && !SEEN[id].swap(true, Ordering::Relaxed) {
                writeln!(trace.lock().unwrap(), "{}", id).unwrap();
            }
        }
        *selected == id
    }
}
"#;
