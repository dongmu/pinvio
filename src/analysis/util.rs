//! Shared utility helpers used by multiple detectors.

use rustc_hir::def_id::DefId;
use rustc_middle::mir::Operand;
use rustc_middle::ty::{self, Ty, TyCtxt, TypingMode};
use rustc_span::Symbol;

pub fn matches_path(tcx: TyCtxt<'_>, def_id: DefId, path: &[&str]) -> bool {
    let actual = tcx.def_path_str(def_id);
    let actual_no_generics = strip_generic_args(&actual);
    let expected = path.join("::");
    actual_no_generics == expected || actual_no_generics.ends_with(&expected)
}

fn strip_generic_args(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    // Stripping "<Foo>" between "::" separators leaves "::::"; collapse to "::".
    let mut result = out;
    loop {
        let next = result.replace("::::", "::");
        if next == result { break; }
        result = next;
    }
    result
}

pub fn is_pin_new_unchecked(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    matches_path(tcx, def_id, &["core", "pin", "Pin", "new_unchecked"])
        || matches_path(tcx, def_id, &["std", "pin", "Pin", "new_unchecked"])
}

pub fn mir_callee_def_id<'tcx>(func: &Operand<'tcx>) -> Option<DefId> {
    let const_op = func.constant()?;
    let ty = const_op.const_.ty();
    match ty.kind() {
        ty::FnDef(def_id, _) => Some(*def_id),
        _ => None,
    }
}

/// Returns true if the type is a reference (`&T`, `&mut T`) or a raw pointer (`*const T`, `*mut T`).
pub fn is_reference_like(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), ty::Ref(..) | ty::RawPtr(..))
}

/// Returns true if `ty` is structurally `!Unpin` via trait resolution.
pub fn is_not_unpin<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>, param_env: ty::ParamEnv<'tcx>) -> bool {
    let unpin_trait = match tcx.lang_items().unpin_trait() {
        Some(t) => t,
        None => return false,
    };
    !type_implements_trait(tcx, ty, unpin_trait, param_env)
}

/// Wrapper around obligation checking to test whether `ty` implements `trait_def_id`.
pub fn type_implements_trait<'tcx>(
    tcx: TyCtxt<'tcx>,
    ty: Ty<'tcx>,
    trait_def_id: DefId,
    param_env: ty::ParamEnv<'tcx>,
) -> bool {
    use rustc_infer::infer::TyCtxtInferExt;
    use rustc_middle::ty::TraitRef;
    use rustc_trait_selection::traits::ObligationCtxt;

    let infcx = tcx.infer_ctxt().build(TypingMode::non_body_analysis());
    let ocx = ObligationCtxt::new(&infcx);
    let cause = rustc_middle::traits::ObligationCause::dummy();
    let trait_ref = TraitRef::new(tcx, trait_def_id, [ty]);
    ocx.register_bound(cause, param_env, ty, trait_def_id);
    let _ = trait_ref;
    ocx.select_all_or_error().is_empty()
}

/// True if `ty` contains `PhantomPinned` anywhere in its field structure (syntactic check).
pub fn contains_phantom_pinned<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
    fn walk<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>, depth: u32) -> bool {
        if depth > 16 {
            return false;
        }
        match ty.kind() {
            ty::Adt(adt_def, args) => {
                let path = tcx.def_path_str(adt_def.did());
                if path == "core::marker::PhantomPinned" || path == "std::marker::PhantomPinned" {
                    return true;
                }
                for variant in adt_def.variants() {
                    for field in &variant.fields {
                        let field_ty = field.ty(tcx, args);
                        if walk(tcx, field_ty, depth + 1) {
                            return true;
                        }
                    }
                }
                false
            }
            ty::Tuple(tys) => tys.iter().any(|t| walk(tcx, t, depth + 1)),
            ty::Array(inner, _) => walk(tcx, *inner, depth + 1),
            _ => false,
        }
    }
    walk(tcx, ty, 0)
}

/// Convenience: intern a string as a rustc symbol.
pub fn sym(s: &str) -> Symbol {
    Symbol::intern(s)
}

pub fn all_items(tcx: TyCtxt<'_>) -> Vec<rustc_hir::ItemId> {
    use rustc_hir as hir;
    use rustc_hir::intravisit::{self, Visitor};
    use rustc_middle::hir::nested_filter;
    use std::collections::HashSet;

    struct Collector<'tcx> {
        tcx: TyCtxt<'tcx>,
        seen: HashSet<hir::ItemId>,
        items: Vec<hir::ItemId>,
    }

    impl<'tcx> Visitor<'tcx> for Collector<'tcx> {
        type NestedFilter = nested_filter::All;

        fn maybe_tcx(&mut self) -> Self::MaybeTyCtxt {
            self.tcx
        }

        fn visit_item(&mut self, item: &'tcx hir::Item<'tcx>) {
            // `seen` guards against double-visits that could occur if
            // `hir_visit_all_item_likes_in_crate` and `walk_item` both enumerate
            // the same item (e.g. for inline or external sub-modules).
            if self.seen.insert(item.item_id()) {
                self.items.push(item.item_id());
            }
            intravisit::walk_item(self, item);
        }
    }

    let mut collector = Collector { tcx, seen: HashSet::new(), items: Vec::new() };
    tcx.hir_visit_all_item_likes_in_crate(&mut collector);
    collector.items
}

/// Walk the entire crate's HIR to find every `Pin::new_unchecked(&mut x.field)`
/// projection site, returning a map from ADT `DefId` to structurally-pinned field names.
///
/// Shared between the missing-unpin-bound and wrong-unpin-direction detectors.
pub fn collect_pinned_fields(
    tcx: TyCtxt<'_>,
) -> std::collections::HashMap<DefId, std::collections::HashSet<String>> {
    use rustc_hir as hir;
    use rustc_hir::intravisit::{self, Visitor};
    use rustc_middle::hir::nested_filter;
    use std::collections::{HashMap, HashSet};

    struct ProjectionFinder<'tcx> {
        tcx: TyCtxt<'tcx>,
        pinned_fields: HashMap<DefId, HashSet<String>>,
    }

    impl<'tcx> Visitor<'tcx> for ProjectionFinder<'tcx> {
        type NestedFilter = nested_filter::All;

        fn maybe_tcx(&mut self) -> Self::MaybeTyCtxt {
            self.tcx
        }

        fn visit_expr(&mut self, expr: &'tcx hir::Expr<'tcx>) {
            if let hir::ExprKind::Call(callee, args) = &expr.kind {
                if self.is_pin_new_unchecked(callee) {
                    if let Some(arg) = args.first() {
                        self.record_if_field_borrow(arg);
                    }
                }
            }
            intravisit::walk_expr(self, expr);
        }
    }

    impl<'tcx> ProjectionFinder<'tcx> {
        fn is_pin_new_unchecked(&self, callee: &hir::Expr<'_>) -> bool {
            let hir::ExprKind::Path(qpath) = &callee.kind else { return false };
            let typeck = self.tcx.typeck(callee.hir_id.owner.def_id);
            let res = typeck.qpath_res(qpath, callee.hir_id);
            let Some(def_id) = res.opt_def_id() else { return false };
            is_pin_new_unchecked(self.tcx, def_id)
        }

        fn record_if_field_borrow(&mut self, arg: &hir::Expr<'_>) {
            let hir::ExprKind::AddrOf(_, _, inner) = arg.kind else { return };
            let (field_name, base) = match inner.kind {
                hir::ExprKind::Field(base, ident) => (ident.name.to_string(), base),
                _ => return,
            };
            let typeck = self.tcx.typeck(arg.hir_id.owner.def_id);
            let mut base_ty = typeck.expr_ty(base);
            loop {
                match base_ty.kind() {
                    ty::Ref(_, inner, _) => base_ty = *inner,
                    ty::Adt(adt_def, _) => {
                        self.pinned_fields
                            .entry(adt_def.did())
                            .or_default()
                            .insert(field_name);
                        return;
                    }
                    _ => return,
                }
            }
        }
    }

    let mut finder = ProjectionFinder {
        tcx,
        pinned_fields: HashMap::new(),
    };
    tcx.hir_visit_all_item_likes_in_crate(&mut finder);
    finder.pinned_fields
}
