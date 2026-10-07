//! Resolve user-facing paths through module re-exports and inherent methods.
//! Rules refer to definitions, so imports and renamed dependencies do not change
//! their meaning. Globs additionally match rustc's printed definition paths.
use rustc_hir::{
    def::{DefKind, Res},
    def_id::{CRATE_DEF_INDEX, DefId, LOCAL_CRATE},
};
use rustc_middle::ty::TyCtxt;
use rustc_span::def_id::ModId;

pub fn resolve(tcx: TyCtxt<'_>, path: &str) -> Vec<DefId> {
    if path.contains('*') {
        return Vec::new();
    }
    let mut segments = path.split("::");
    let Some(crate_name) = segments.next() else {
        return Vec::new();
    };
    let mut candidates: Vec<_> = std::iter::once(LOCAL_CRATE)
        .chain(tcx.crates(()).iter().copied())
        .filter(|&krate| tcx.crate_name(krate).as_str() == crate_name)
        .map(|krate| DefId {
            krate,
            index: CRATE_DEF_INDEX,
        })
        .collect();
    for segment in segments {
        let mut next = Vec::new();
        for parent in candidates {
            match tcx.def_kind(parent) {
                DefKind::Mod => {
                    let children = if let Some(local) = parent.as_local() {
                        tcx.module_children_local(local)
                    } else {
                        tcx.module_children(ModId::new_unchecked(parent))
                    };
                    for child in children {
                        if child.ident.name.as_str() == segment
                            && let Res::Def(_, id) = child.res
                        {
                            next.push(id);
                        }
                    }
                }
                DefKind::Struct | DefKind::Enum | DefKind::Union | DefKind::TyAlias => {
                    let ty = tcx.type_of(parent).instantiate_identity().skip_norm_wip();
                    if let rustc_middle::ty::Adt(adt, _) = ty.kind() {
                        for &implementation in tcx.inherent_impls(adt.did()) {
                            next.extend(
                                tcx.associated_items(implementation)
                                    .in_definition_order()
                                    .filter(|item| item.name().as_str() == segment)
                                    .map(|item| item.def_id),
                            );
                        }
                    }
                }
                DefKind::Trait => {
                    next.extend(
                        tcx.associated_items(parent)
                            .in_definition_order()
                            .filter(|item| item.name().as_str() == segment)
                            .map(|item| item.def_id),
                    );
                }
                _ => {}
            }
        }
        next.sort_by_key(|id| (id.krate.as_u32(), id.index.as_u32()));
        next.dedup();
        candidates = next;
    }
    candidates
}
