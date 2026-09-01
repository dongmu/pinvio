use super::util;
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::Span;
use std::collections::{HashMap, HashSet};

pub fn check_crate(tcx: TyCtxt<'_>) {
    let pinned_fields = util::collect_pinned_fields(tcx);
    if pinned_fields.is_empty() {
        return;
    }
    let detector = Detector { tcx, pinned_fields };
    detector.scan_methods();
}

struct Detector<'tcx> {
    tcx: TyCtxt<'tcx>,
    pinned_fields: HashMap<DefId, HashSet<String>>,
}

impl<'tcx> Detector<'tcx> {
    fn scan_methods(&self) {
        for item_id in util::all_items(self.tcx) {
            let item = self.tcx.hir_item(item_id);
            let hir::ItemKind::Impl(impl_data) = item.kind else { continue };

            // Only inherent impls; trait impls have bounds determined by the trait.
            if impl_data.of_trait.is_some() {
                continue;
            }

            let self_ty = self.tcx.type_of(item.owner_id.def_id).instantiate_identity();
            let ty::Adt(adt_def, _) = self_ty.kind() else { continue };
            let adt_did = adt_def.did();

            let Some(pinned) = self.pinned_fields.get(&adt_did) else { continue };
            if pinned.is_empty() {
                continue;
            }

            for impl_item_ref in impl_data.items {
                let assoc = self.tcx.hir_expect_impl_item(impl_item_ref.owner_id.def_id);
                let hir::ImplItemKind::Fn(sig, _) = &assoc.kind else { continue };

                // Unsafe methods may legitimately un-pin without the bound.
                if sig.header.safety() == hir::Safety::Unsafe {
                    continue;
                }

                self.check_method(*adt_def, pinned, *impl_item_ref, assoc, &sig);
            }
        }
    }

    fn check_method(
        &self,
        adt_def: ty::AdtDef<'tcx>,
        pinned_fields: &HashSet<String>,
        impl_item_id: hir::ImplItemId,
        assoc: &hir::ImplItem<'tcx>,
        sig: &hir::FnSig<'tcx>,
    ) {
        let method_did = assoc.owner_id.to_def_id();
        let fn_sig = self.tcx.fn_sig(method_did).instantiate_identity().skip_binder();

        let self_param_names: HashMap<u32, String> = self
            .tcx
            .generics_of(adt_def.did())
            .own_params
            .iter()
            .filter(|p| matches!(p.kind, ty::GenericParamDefKind::Type { .. }))
            .map(|p| (p.index, p.name.to_string()))
            .collect();

        let variant = adt_def.non_enum_variant();
        let pinned_field_info: Vec<PinnedFieldInfo> = variant
            .fields
            .iter()
            .filter_map(|field| {
                let name = field.name.to_string();
                if !pinned_fields.contains(&name) {
                    return None;
                }
                let field_ty = self.tcx.type_of(field.did).instantiate_identity();
                let param_name = match field_ty.kind() {
                    ty::Param(p) => self_param_names.get(&p.index).cloned(),
                    _ => None,
                };
                Some(PinnedFieldInfo { name, ty: field_ty, param_name })
            })
            .collect();

        if pinned_field_info.is_empty() {
            return;
        }

        let return_ty = fn_sig.output();
        for info in &pinned_field_info {
            if self.return_exposes_field_ty(return_ty, info.ty) {
                if !self.method_bounds_field_unpin(method_did, info) {
                    self.report_return_hazard(assoc.span, &info.name, info, sig);
                    return;
                }
            }
        }

        if self.takes_mut_self_not_pin(sig)
            && self.body_projects_through_pinned_field(impl_item_id, pinned_fields)
        {
            for info in &pinned_field_info {
                if !self.method_bounds_field_unpin(method_did, info) {
                    self.report_projection_hazard(assoc.span, info, sig);
                    return;
                }
            }
        }
    }

    fn return_exposes_field_ty(&self, return_ty: Ty<'tcx>, field_ty: Ty<'tcx>) -> bool {
        if return_ty == field_ty {
            return true;
        }
        match return_ty.kind() {
            ty::Adt(adt_def, args) => {
                for variant in adt_def.variants() {
                    for field in &variant.fields {
                        if field.ty(self.tcx, args) == field_ty {
                            return true;
                        }
                    }
                }
                false
            }
            ty::Tuple(tys) => tys.iter().any(|t| t == field_ty),
            _ => false,
        }
    }

    fn takes_mut_self_not_pin(&self, sig: &hir::FnSig<'_>) -> bool {
        let Some(first_input) = sig.decl.inputs.first() else { return false };
        match &first_input.kind {
            hir::TyKind::Ref(_, mut_ty) => mut_ty.mutbl == hir::Mutability::Mut,
            _ => false,
        }
    }

    fn body_projects_through_pinned_field(
        &self,
        impl_item_id: hir::ImplItemId,
        _pinned_fields: &HashSet<String>,
    ) -> bool {
        use rustc_hir::intravisit::{self, Visitor};
        use rustc_middle::hir::nested_filter;

        struct Finder<'tcx> {
            tcx: TyCtxt<'tcx>,
            found: bool,
        }
        impl<'tcx> Visitor<'tcx> for Finder<'tcx> {
            type NestedFilter = nested_filter::OnlyBodies;
            fn maybe_tcx(&mut self) -> Self::MaybeTyCtxt {
                self.tcx
            }
            fn visit_expr(&mut self, expr: &'tcx hir::Expr<'tcx>) {
                if self.found {
                    return;
                }
                if let hir::ExprKind::Call(callee, _) = expr.kind {
                    if let hir::ExprKind::Path(qpath) = &callee.kind {
                        let typeck = self.tcx.typeck(callee.hir_id.owner.def_id);
                        let res = typeck.qpath_res(qpath, callee.hir_id);
                        if let Some(def_id) = res.opt_def_id() {
                            if util::matches_path(
                                self.tcx,
                                def_id,
                                &["core", "pin", "Pin", "new_unchecked"],
                            ) || util::matches_path(
                                self.tcx,
                                def_id,
                                &["std", "pin", "Pin", "new_unchecked"],
                            ) {
                                self.found = true;
                                return;
                            }
                        }
                    }
                }
                intravisit::walk_expr(self, expr);
            }
        }

        let mut finder = Finder { tcx: self.tcx, found: false };
        let assoc = self.tcx.hir_expect_impl_item(impl_item_id.owner_id.def_id);
        if let hir::ImplItemKind::Fn(_, body_id) = assoc.kind {
            let body = self.tcx.hir_body(body_id);
            finder.visit_expr(body.value);
        }
        finder.found
    }

    fn method_bounds_field_unpin(&self, method_did: DefId, info: &PinnedFieldInfo<'tcx>) -> bool {
        let predicates = self.tcx.predicates_of(method_did);
        let unpin_trait = match self.tcx.lang_items().unpin_trait() {
            Some(t) => t,
            None => return false,
        };

        for (pred, _span) in predicates.predicates {
            let Some(clause) = pred.as_trait_clause() else { continue };
            let trait_ref = clause.skip_binder().trait_ref;
            if trait_ref.def_id != unpin_trait {
                continue;
            }
            let bounded_ty = trait_ref.self_ty();
            if bounded_ty == info.ty {
                return true;
            }
            if let (ty::Param(field_p), ty::Param(bound_p)) = (info.ty.kind(), bounded_ty.kind()) {
                if field_p.index == bound_p.index {
                    return true;
                }
            }
        }
        false
    }

    fn report_return_hazard(
        &self,
        span: Span,
        field_name: &str,
        info: &PinnedFieldInfo,
        _sig: &hir::FnSig<'_>,
    ) {
        let bound_target = match &info.param_name {
            Some(p) => p.clone(),
            None => format!("{}", info.ty),
        };
        self.tcx
            .dcx()
            .struct_span_warn(
                span,
                "method returns a structurally-pinned field's type by value without an `Unpin` bound",
            )
            .with_note(format!(
                "field `{field_name}` is structurally pinned (it is the target of \
                 `Pin::new_unchecked` projection elsewhere on this type), so any \
                 method that exposes it by ownership requires `{bound_target}: Unpin` \
                 in its where-clause"
            ))
            .with_note(
                "without this bound, callers can pin the field through one method, \
                 then move it via this method — see RUSTSEC-2023-0005 (tokio `unsplit`)",
            )
            .with_help(format!(
                "add `where {bound_target}: Unpin` to this method's signature, or \
                 mark the method `unsafe`",
            ))
            .emit();
    }

    fn report_projection_hazard(&self, span: Span, info: &PinnedFieldInfo, _sig: &hir::FnSig<'_>) {
        let bound_target = match &info.param_name {
            Some(p) => p.clone(),
            None => format!("{}", info.ty),
        };
        self.tcx
            .dcx()
            .struct_span_warn(
                span,
                "method takes `&mut self` and projects through a pinned field \
                 without an `Unpin` bound",
            )
            .with_note(format!(
                "the body calls `Pin::new_unchecked` on a field whose type \
                 (`{bound_target}`) is treated as structurally pinned elsewhere, \
                 but this method's receiver is `&mut self` rather than \
                 `Pin<&mut Self>` — pinning here does not retroactively pin \
                 the underlying storage"
            ))
            .with_note("see tokio PR #2612 (`AsyncReadExt::read_buf` / `write_buf`)")
            .with_help(format!(
                "either add `where Self: Unpin` (or `where {bound_target}: Unpin`) \
                 to this method, change the receiver to `Pin<&mut Self>`, or mark \
                 the method `unsafe`",
            ))
            .emit();
    }
}

struct PinnedFieldInfo<'tcx> {
    name: String,
    ty: Ty<'tcx>,
    param_name: Option<String>,
}
