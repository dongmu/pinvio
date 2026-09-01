use super::util;
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::Span;
use std::collections::{HashMap, HashSet};

pub fn check_crate(tcx: TyCtxt<'_>) {
    let mut detector = Detector::new(tcx);
    detector.pinned_fields = util::collect_pinned_fields(tcx);
    detector.scan_unpin_impls();
}

struct Detector<'tcx> {
    tcx: TyCtxt<'tcx>,
    pinned_fields: HashMap<DefId, HashSet<String>>,
}

impl<'tcx> Detector<'tcx> {
    fn new(tcx: TyCtxt<'tcx>) -> Self {
        Self { tcx, pinned_fields: HashMap::new() }
    }

    fn scan_unpin_impls(&self) {
        for item_id in util::all_items(self.tcx) {
            let item = self.tcx.hir_item(item_id);
            let hir::ItemKind::Impl(impl_data) = item.kind else { continue };

            let Some(of_trait) = impl_data.of_trait else { continue };
            let Some(trait_def_id) = of_trait.trait_def_id() else { continue };
            if Some(trait_def_id) != self.tcx.lang_items().unpin_trait() {
                continue;
            }

            let self_ty = self.tcx.type_of(item.owner_id.def_id).instantiate_identity();
            let ty::Adt(adt_def, _) = self_ty.kind() else { continue };
            let adt_did = adt_def.did();

            let pinned = match self.pinned_fields.get(&adt_did) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };

            self.verify_where_clause(item, impl_data, *adt_def, pinned);
        }
    }

    fn verify_where_clause(
        &self,
        item: &hir::Item<'_>,
        impl_data: &hir::Impl<'_>,
        adt_def: ty::AdtDef<'_>,
        pinned_fields: &HashSet<String>,
    ) {
        let mut unpin_bounded_params: HashSet<String> = HashSet::new();
        for predicate in impl_data.generics.predicates {
            let hir::WherePredicateKind::BoundPredicate(bp) = predicate.kind else { continue };
            let bounded_ty_str = self.format_hir_ty(bp.bounded_ty);
            for bound in bp.bounds {
                if self.bound_is_unpin(bound) {
                    unpin_bounded_params.insert(bounded_ty_str.clone());
                }
            }
        }

        let variant = adt_def.non_enum_variant();
        for field in &variant.fields {
            let field_name = field.name.to_string();
            if !pinned_fields.contains(&field_name) {
                continue;
            }
            let field_ty = self.tcx.type_of(field.did).instantiate_identity();

            if let ty::Param(p) = field_ty.kind() {
                if !unpin_bounded_params.contains(p.name.as_str()) {
                    self.report(
                        item.span,
                        &field_name,
                        &p.name.to_string(),
                        &unpin_bounded_params,
                    );
                }
            }
        }
    }

    fn bound_is_unpin(&self, bound: &hir::GenericBound<'_>) -> bool {
        let hir::GenericBound::Trait(poly_ref) = bound else { return false };
        let Some(trait_def_id) = poly_ref.trait_ref.trait_def_id() else { return false };
        Some(trait_def_id) == self.tcx.lang_items().unpin_trait()
    }

    fn format_hir_ty(&self, ty: &hir::Ty<'_>) -> String {
        if let hir::TyKind::Path(hir::QPath::Resolved(_, path)) = &ty.kind {
            if let Some(seg) = path.segments.last() {
                return seg.ident.name.to_string();
            }
        }
        format!("{ty:?}")
    }

    fn report(
        &self,
        span: Span,
        pinned_field: &str,
        pinned_field_param: &str,
        actually_bounded: &HashSet<String>,
    ) {
        let actually = if actually_bounded.is_empty() {
            "(none)".to_string()
        } else {
            let mut v: Vec<_> = actually_bounded.iter().collect();
            v.sort();
            v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        };
        self.tcx
            .dcx()
            .struct_span_warn(
                span,
                "wrong-direction `Unpin` bound: structurally-pinned field is not bounded by `Unpin`",
            )
            .with_note(format!(
                "field `{pinned_field}` (type parameter `{pinned_field_param}`) is structurally \
                 pinned (it is the target of `Pin::new_unchecked` projection somewhere), \
                 so the impl must require `{pinned_field_param}: Unpin`"
            ))
            .with_note(format!("instead, the where-clause bounds: {actually}"))
            .with_help(
                "either change the where-clause to bound the pinned field's type parameter, \
                 or use the `pin_project` macro which emits the correct bounds automatically",
            )
            .emit();
    }
}
