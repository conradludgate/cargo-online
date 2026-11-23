//! Split externally linkable local function instances into incremental work products.
use rustc_attr_ir::Linkage;
use rustc_hir::def::DefKind;
use rustc_middle::{
    mono::{CodegenUnit, CollectionMode, MonoItem, MonoItemPartitions, Visibility},
    ty::{InstanceKind, TyCtxt},
};
use rustc_span::Symbol;
use std::sync::OnceLock;

pub type Provider = for<'tcx> fn(TyCtxt<'tcx>, ()) -> MonoItemPartitions<'tcx>;
pub static ORIGINAL: OnceLock<Provider> = OnceLock::new();

fn independent<'tcx>(tcx: TyCtxt<'tcx>, unit: &CodegenUnit<'tcx>, item: MonoItem<'tcx>) -> bool {
    let MonoItem::Fn(instance) = item else {
        return false;
    };
    let InstanceKind::Item(def) = instance.def else {
        return false;
    };
    if !def.is_local() || !matches!(tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn) {
        return false;
    }
    let data = &unit.items()[&item];
    let attrs = tcx.codegen_instance_attrs(instance.def);
    if attrs.symbol_name.is_some()
        || attrs.link_section.is_some()
        || !attrs.foreign_item_symbol_aliases.is_empty()
        || attrs
            .flags
            .contains(rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags::NAKED)
    {
        return false;
    }
    if data.inlined
        || item.explicit_linkage(tcx).is_some()
        || !matches!(data.linkage, Linkage::External | Linkage::Internal)
    {
        return false;
    }
    // A unique home definition can be referenced externally. Every outgoing
    // reference must also be legal from a different object file.
    tcx.items_of_instance((instance, CollectionMode::UsedItems))
        .is_ok_and(|(used, _)| {
            used.iter().all(|dep| {
                unit.items()
                    .get(&dep.node)
                    .is_none_or(|d| !d.inlined && d.linkage == Linkage::External)
            })
        })
}

pub fn partition(tcx: TyCtxt<'_>, (): ()) -> MonoItemPartitions<'_> {
    let original = ORIGINAL.get().unwrap()(tcx, ());
    let mut units = Vec::new();
    let mut split = 0;
    for unit in original.codegen_units {
        let mut remainder = CodegenUnit::new(unit.name());
        remainder.set_symbol_name(unit.symbol_name());
        if unit.is_primary() {
            remainder.make_primary();
        }
        if unit.is_code_coverage_dead_code_cgu() {
            remainder.make_code_coverage_dead_code_cgu();
        }
        let eligible = !unit.is_code_coverage_dead_code_cgu()
            && !unit
                .items()
                .keys()
                .any(|i| matches!(i, MonoItem::GlobalAsm(_)));
        for (&item, &original_data) in unit.items() {
            let mut data = original_data;
            if eligible && independent(tcx, unit, item) {
                let key = format!(
                    "{:?}:{}",
                    tcx.stable_crate_id(rustc_hir::def_id::LOCAL_CRATE),
                    item.symbol_name(tcx)
                );
                let digest = CodegenUnit::mangle_name(&key);
                let name = Symbol::intern(&format!("co_{digest}"));
                let mut isolated = CodegenUnit::new(name);
                isolated.set_symbol_name(name);
                // Undo rustc partitioning's internalization of a unique home definition.
                if data.linkage == Linkage::Internal {
                    data.linkage = Linkage::External;
                    data.visibility = Visibility::Hidden;
                }
                isolated.items_mut().insert(item, data);
                isolated.compute_size_estimate();
                units.push(isolated);
                split += 1;
            } else {
                remainder.items_mut().insert(item, data);
            }
        }
        remainder.compute_size_estimate();
        units.push(remainder);
    }
    if split == 0 {
        return original;
    }
    units.sort_by(|a, b| {
        b.is_primary()
            .cmp(&a.is_primary())
            .then_with(|| a.name().as_str().cmp(b.name().as_str()))
    });
    eprintln!(
        "cargo-online: isolated {split} function instances in {} codegen units",
        units.len()
    );
    MonoItemPartitions {
        codegen_units: tcx.arena.alloc_from_iter(units),
        ..original
    }
}
