//! Plan edits from cargo-mutants' JSON catalog against an immutable source snapshot.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    ops::Range,
    path::{Component, Path, PathBuf},
};
use syn::{
    spanned::Spanned,
    visit::{self, Visit},
};

#[derive(Deserialize)]
struct Position {
    line: usize,
    column: usize,
}
#[derive(Deserialize)]
struct Span {
    start: Position,
    end: Position,
}
#[derive(Deserialize)]
pub struct Candidate {
    pub name: String,
    pub package: String,
    pub file: PathBuf,
    span: Span,
    replacement: String,
}

pub struct Edit {
    pub name: String,
    pub package: String,
    pub file: PathBuf,
    range: Range<usize>,
    replacement: String,
    region: Option<Range<usize>>,
}

#[derive(Default)]
struct Regions {
    spans: Vec<Range<usize>>,
}

// Dispatch arms share a type inference context. An opaque return type must
// be inferred independently for each edit, just as in a standalone build.
fn runtime_signature(sig: &syn::Signature) -> bool {
    #[derive(Default)]
    struct Opaque(bool);
    impl<'a> Visit<'a> for Opaque {
        fn visit_type_impl_trait(&mut self, _: &'a syn::TypeImplTrait) {
            self.0 = true;
        }
    }
    let mut opaque = Opaque::default();
    opaque.visit_return_type(&sig.output);
    sig.constness.is_none() && !opaque.0
}

impl<'a> Visit<'a> for Regions {
    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        if runtime_signature(&f.sig) {
            self.spans.push(f.block.span().byte_range());
        }
        visit::visit_item_fn(self, f);
    }
    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        if runtime_signature(&f.sig) {
            self.spans.push(f.block.span().byte_range());
        }
        visit::visit_impl_item_fn(self, f);
    }
    fn visit_trait_item_fn(&mut self, f: &'a syn::TraitItemFn) {
        if runtime_signature(&f.sig)
            && let Some(body) = &f.default
        {
            self.spans.push(body.span().byte_range());
        }
        visit::visit_trait_item_fn(self, f);
    }
}

struct Source {
    text: String,
    lines: Vec<usize>,
    regions: Vec<Range<usize>>,
}

impl Source {
    fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let lines = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let mut regions = Regions::default();
        if let Ok(file) = syn::parse_file(&text) {
            let prefix = usize::from(text.starts_with('\u{feff}')) * 3
                + file.shebang.as_ref().map_or(0, String::len);
            regions.visit_file(&file);
            for r in &mut regions.spans {
                r.start += prefix;
                r.end += prefix;
            }
        }
        Ok(Self {
            text,
            lines,
            regions: regions.spans,
        })
    }
    fn offset(&self, pos: &Position) -> Result<usize> {
        let start = *self
            .lines
            .get(pos.line.checked_sub(1).context("zero line")?)
            .context("line outside source")?;
        let end = self.text[start..]
            .find('\n')
            .map_or(self.text.len(), |n| start + n);
        let line = self.text[start..end].trim_end_matches('\r');
        let column = pos.column.checked_sub(1).context("zero column")?;
        let byte = line
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(line.len()))
            .nth(column)
            .context("column outside source")?;
        Ok(start + byte)
    }
}
pub struct Catalog {
    sources: BTreeMap<PathBuf, Source>,
    pub edits: Vec<Edit>,
}
impl Catalog {
    pub fn new(root: &Path, candidates: Vec<Candidate>) -> Result<Self> {
        let mut sources = BTreeMap::new();
        let mut edits = Vec::new();
        for c in candidates {
            ensure!(
                c.file
                    .components()
                    .all(|p| matches!(p, Component::Normal(_) | Component::CurDir)),
                "catalog path outside workspace"
            );
            let file = root
                .join(&c.file)
                .canonicalize()?
                .strip_prefix(root)?
                .to_path_buf();
            let source = match sources.entry(file.clone()) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(Source::read(&root.join(&c.file))?)
                }
            };
            let range = source.offset(&c.span.start)?..source.offset(&c.span.end)?;
            ensure!(
                range.start <= range.end,
                "invalid catalog span for {}",
                c.name
            );
            let region = source
                .regions
                .iter()
                .filter(|r| r.start <= range.start && range.end <= r.end)
                .min_by_key(|r| r.len())
                .cloned();
            edits.push(Edit {
                name: c.name,
                package: c.package,
                file,
                range,
                replacement: c.replacement,
                region,
            });
        }
        Ok(Self { sources, edits })
    }
    pub fn restore(&self, root: &Path, ids: &[usize]) -> Result<()> {
        let paths: std::collections::BTreeSet<_> =
            ids.iter().map(|&i| &self.edits[i].file).collect();
        for path in paths {
            fs::write(root.join(path), &self.sources[path].text)?;
        }
        Ok(())
    }
    pub fn literal(&self, root: &Path, id: usize) -> Result<()> {
        let e = &self.edits[id];
        let mut s = self.sources[&e.file].text.clone();
        s.replace_range(e.range.clone(), &e.replacement);
        fs::write(root.join(&e.file), s)?;
        Ok(())
    }
    pub fn bundleable(&self, id: usize) -> bool {
        self.edits[id].region.is_some()
    }
    pub fn bundle(&self, root: &Path, ids: &[usize]) -> Result<()> {
        let mut files = BTreeMap::<_, Vec<_>>::new();
        for &id in ids {
            files.entry(&self.edits[id].file).or_default().push(id);
        }
        for (path, members) in files {
            let source = &self.sources[path].text;
            let mut regions = BTreeMap::<_, Vec<_>>::new();
            for id in members {
                let r = self.edits[id]
                    .region
                    .as_ref()
                    .context("missing dispatch region")?;
                regions.entry((r.start, r.end)).or_default().push(id);
            }
            // Fold nested regions into their parents. Only the baseline arm needs
            // inner switches because each process activates at most one mutant.
            let mut regions: Vec<_> = regions.into_iter().collect();
            regions.sort_by_key(|((a, b), _)| (*a, std::cmp::Reverse(*b)));
            let mut fragments: Vec<(Range<usize>, String)> = Vec::new();
            for ((a, b), variants) in regions.into_iter().rev() {
                let mut baseline = source[a..b].to_owned();
                let first = fragments
                    .iter()
                    .position(|(r, _)| r.start < b)
                    .unwrap_or(fragments.len());
                for (r, text) in fragments.drain(first..) {
                    ensure!(r.start >= a && r.end <= b, "overlapping dispatch regions");
                    baseline.replace_range(r.start - a..r.end - a, &text);
                }
                // Local extern crate also works in host-tested no_std packages
                // and in nested modules, integration tests, and binary targets.
                let hits = variants
                    .iter()
                    .map(|id| format!("{}\n", id + 1))
                    .collect::<String>();
                let mut switch = SELECTOR.replace("TRACE_IDS", &format!("{hits:?}"));
                for id in variants {
                    let edit = &self.edits[id];
                    let mut changed = source[a..b].to_owned();
                    changed
                        .replace_range(edit.range.start - a..edit.range.end - a, &edit.replacement);
                    switch.push_str(&format!(
                        "{} => {{ /*cargo-online:{id}*/ {changed} /*cargo-online:end*/ }},",
                        id + 1
                    ));
                }
                switch.push_str(&format!("_ => {{ {baseline} }} }} }}"));
                fragments.push((a..b, switch));
            }
            let mut output = source.clone();
            for (r, s) in fragments {
                output.replace_range(r, &s);
            }
            fs::write(root.join(path), output)?;
        }
        Ok(())
    }
}

// Use one cached selection per function. In coverage mode one append records
// every variant of that function on its first entry in this test process.
const SELECTOR: &str = r#"{ extern crate std as __cargo_online_std;
static __CARGO_ONLINE_ID: __cargo_online_std::sync::OnceLock<usize> = __cargo_online_std::sync::OnceLock::new();
match *__CARGO_ONLINE_ID.get_or_init(|| {
    if let Some(path) = __cargo_online_std::env::var_os("CARGO_ONLINE_TRACE") {
        use __cargo_online_std::io::Write as _;
        __cargo_online_std::fs::OpenOptions::new().append(true).open(path).unwrap()
            .write_all(TRACE_IDS.as_bytes()).unwrap();
    }
    __cargo_online_std::env::var("CARGO_ONLINE_MUTANT").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}) {"#;
