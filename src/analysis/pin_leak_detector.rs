use super::util;
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_index::IndexVec;
use rustc_middle::mir::{
    Body, Local, Location, Operand, Rvalue, Statement, StatementKind,
    Terminator, TerminatorEdges, TerminatorKind,
};
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_mir_dataflow::fmt::DebugWithContext;
use rustc_mir_dataflow::{Analysis, JoinSemiLattice, ResultsVisitor, visit_reachable_results};
use rustc_span::Span;
use std::collections::HashSet;

pub fn check_crate(tcx: TyCtxt<'_>) {
    let detector = Detector::new(tcx);
    let drop_critical = detector.classify_drop_critical_types();
    if drop_critical.is_empty() {
        return;
    }
    detector.scan_constructors(&drop_critical);
}

struct Detector<'tcx> {
    tcx: TyCtxt<'tcx>,
}

impl<'tcx> Detector<'tcx> {
    fn new(tcx: TyCtxt<'tcx>) -> Self {
        Self { tcx }
    }

    fn classify_drop_critical_types(&self) -> HashSet<DefId> {
        let mut result = HashSet::new();
        for item_id in util::all_items(self.tcx) {
            let item = self.tcx.hir_item(item_id);
            if !matches!(
                item.kind,
                hir::ItemKind::Struct(..) | hir::ItemKind::Enum(..) | hir::ItemKind::Union(..)
            ) {
                continue;
            }
            let did = item.owner_id.to_def_id();
            let ty = self.tcx.type_of(did).instantiate_identity();
            if self.is_drop_critical(ty, did) {
                result.insert(did);
            }
        }
        result
    }

    fn is_drop_critical(&self, ty: Ty<'tcx>, ty_did: DefId) -> bool {
        if !self.has_drop_impl(ty_did) {
            return false;
        }
        if util::contains_phantom_pinned(self.tcx, ty) {
            return true;
        }
        self.drop_impl_does_external_cleanup(ty_did)
    }

    fn has_drop_impl(&self, ty_did: DefId) -> bool {
        self.tcx.adt_destructor(ty_did).is_some()
    }

    fn drop_impl_does_external_cleanup(&self, ty_did: DefId) -> bool {
        let Some(destructor) = self.tcx.adt_destructor(ty_did) else {
            return false;
        };
        let drop_method_did = destructor.did;
        if drop_method_did.as_local().is_none() {
            return false;
        }
        let body = self.tcx.optimized_mir(drop_method_did);

        for data in body.basic_blocks.iter() {
            let Some(term) = &data.terminator else { continue };
            let TerminatorKind::Call { func, .. } = &term.kind else { continue };
            let Some(callee) = util::mir_callee_def_id(func) else { continue };
            let path = self.tcx.def_path_str(callee);

            const CLEANUP_KEYWORDS: &[&str] = &[
                "::close", "::release", "::deregister", "::unregister",
                "::lock", "::unlock", "::retain", "::remove",
                "::wait", "::join", "::shutdown", "::abort",
            ];
            for kw in CLEANUP_KEYWORDS {
                if path.contains(kw) {
                    return true;
                }
            }
        }
        false
    }

    fn scan_constructors(&self, drop_critical: &HashSet<DefId>) {
        for item_id in util::all_items(self.tcx) {
            let item = self.tcx.hir_item(item_id);
            match &item.kind {
                hir::ItemKind::Fn { sig, .. } => {
                    if self.should_check_fn(item.owner_id.def_id, sig) {
                        self.check_function(
                            item.owner_id.to_def_id(),
                            drop_critical,
                            item.span,
                        );
                    }
                }
                hir::ItemKind::Impl(impl_data) => {
                    if impl_data.of_trait.is_some() {
                        continue;
                    }
                    for impl_item_ref in impl_data.items {
                        let assoc = self.tcx.hir_expect_impl_item(impl_item_ref.owner_id.def_id);
                        let hir::ImplItemKind::Fn(sig, _) = &assoc.kind else { continue };
                        if self.should_check_fn(assoc.owner_id.def_id, &sig) {
                            self.check_function(
                                assoc.owner_id.to_def_id(),
                                drop_critical,
                                assoc.span,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn should_check_fn(&self, def_id: hir::def_id::LocalDefId, sig: &hir::FnSig<'_>) -> bool {
        if sig.header.safety() == hir::Safety::Unsafe {
            return false;
        }
        self.tcx.visibility(def_id).is_public()
    }

    fn check_function(
        &self,
        fn_def_id: DefId,
        drop_critical: &HashSet<DefId>,
        span: Span,
    ) {
        let fn_sig = self
            .tcx
            .fn_sig(fn_def_id)
            .instantiate_identity()
            .skip_binder();
        let return_ty = fn_sig.output();
        let returns_drop_critical = match return_ty.kind() {
            ty::Adt(adt_def, _) => drop_critical.contains(&adt_def.did()),
            _ => false,
        };
        if !returns_drop_critical {
            return;
        }

        let Some(_) = fn_def_id.as_local() else { return };
        let body = self.tcx.optimized_mir(fn_def_id);

        let analysis = OriginAnalysis {
            tcx: self.tcx,
            body,
            drop_critical,
        };
        let mut r = analysis.iterate_to_fixpoint(self.tcx, body, None);

        let mut visitor = ReturnVisitor {
            tcx: self.tcx,
            body,
            drop_critical,
            fn_span: span,
            reported: false,
        };
        visit_reachable_results(body, &mut r.analysis, &r.results, &mut visitor);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    Unknown,
    FreshInThisFn,
    FromParam,
    Mixed,
}

impl Origin {
    fn join(self, other: Origin) -> Origin {
        use Origin::*;
        match (self, other) {
            (Unknown, x) | (x, Unknown) => x,
            (a, b) if a == b => a,
            _ => Mixed,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct OriginMap(IndexVec<Local, Origin>);

impl JoinSemiLattice for OriginMap {
    fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            let new = a.join(*b);
            if new != *a {
                *a = new;
                changed = true;
            }
        }
        changed
    }
}

struct OriginAnalysis<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
    drop_critical: &'a HashSet<DefId>,
}

impl<'a, 'tcx> Analysis<'tcx> for OriginAnalysis<'a, 'tcx> {
    type Domain = OriginMap;
    const NAME: &'static str = "pin_leak_origin";

    fn bottom_value(&self, body: &Body<'tcx>) -> Self::Domain {
        OriginMap(IndexVec::from_elem_n(
            Origin::Unknown,
            body.local_decls.len(),
        ))
    }

    fn initialize_start_block(&self, body: &Body<'tcx>, state: &mut Self::Domain) {
        for arg_local in body.args_iter() {
            state.0[arg_local] = Origin::FromParam;
        }
    }

    fn apply_primary_statement_effect(
        &mut self,
        state: &mut Self::Domain,
        statement: &Statement<'tcx>,
        _location: Location,
    ) {
        let StatementKind::Assign(box (lhs_place, rvalue)) = &statement.kind else {
            return;
        };
        let Some(lhs_local) = lhs_place.as_local() else { return };

        let lhs_ty = self.body.local_decls[lhs_local].ty;
        let lhs_is_drop_critical = self.is_drop_critical_ty(lhs_ty);

        let new_origin = match rvalue {
            Rvalue::Aggregate(_, _) => {
                if lhs_is_drop_critical {
                    Origin::FreshInThisFn
                } else {
                    Origin::Unknown
                }
            }
            Rvalue::Use(operand) => match operand {
                Operand::Move(p) | Operand::Copy(p) => state.0[p.local],
                Operand::Constant(_) => {
                    if lhs_is_drop_critical {
                        Origin::FreshInThisFn
                    } else {
                        Origin::Unknown
                    }
                }
            },
            _ => Origin::Unknown,
        };
        state.0[lhs_local] = new_origin;
    }

    fn apply_primary_terminator_effect<'mir>(
        &mut self,
        state: &mut Self::Domain,
        terminator: &'mir Terminator<'tcx>,
        _location: Location,
    ) -> TerminatorEdges<'mir, 'tcx> {
        if let TerminatorKind::Call { destination, func, .. } = &terminator.kind {
            if let Some(dest_local) = destination.as_local() {
                let return_ty = self.callee_return_ty(func);
                let origin = if return_ty.is_some_and(|ty| self.is_drop_critical_ty(ty)) {
                    Origin::FreshInThisFn
                } else {
                    Origin::Unknown
                };
                state.0[dest_local] = origin;
            }
        }
        terminator.edges()
    }
}

impl<'a, 'tcx> OriginAnalysis<'a, 'tcx> {
    fn is_drop_critical_ty(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            ty::Adt(adt_def, _) => self.drop_critical.contains(&adt_def.did()),
            _ => false,
        }
    }

    fn callee_return_ty(&self, func: &Operand<'tcx>) -> Option<Ty<'tcx>> {
        let const_op = func.constant()?;
        let ty = const_op.const_.ty();
        match ty.kind() {
            ty::FnDef(def_id, args) => {
                let sig = self
                    .tcx
                    .fn_sig(*def_id)
                    .instantiate(self.tcx, args)
                    .skip_binder();
                Some(sig.output())
            }
            _ => None,
        }
    }
}

struct ReturnVisitor<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
    drop_critical: &'a HashSet<DefId>,
    fn_span: Span,
    reported: bool,
}

impl<C> DebugWithContext<C> for OriginMap {}

impl<'a, 'tcx> ResultsVisitor<'tcx, OriginAnalysis<'a, 'tcx>>
    for ReturnVisitor<'a, 'tcx>
{
    fn visit_after_early_terminator_effect(
        &mut self,
        _analysis: &mut OriginAnalysis<'a, 'tcx>,
        state: &OriginMap,
        terminator: &Terminator<'tcx>,
        _location: Location,
    ) {
        if self.reported {
            return;
        }
        if !matches!(terminator.kind, TerminatorKind::Return) {
            return;
        }
        let return_local = Local::from_u32(0);
        let return_ty = self.body.local_decls[return_local].ty;
        let is_drop_critical = match return_ty.kind() {
            ty::Adt(adt_def, _) => self.drop_critical.contains(&adt_def.did()),
            _ => false,
        };
        if !is_drop_critical {
            return;
        }
        if state.0[return_local] != Origin::FreshInThisFn {
            return;
        }
        self.reported = true;
        self.report(return_ty);
    }
}

impl<'a, 'tcx> ReturnVisitor<'a, 'tcx> {
    fn report(&self, return_ty: Ty<'tcx>) {
        self.tcx
            .dcx()
            .struct_span_warn(
                self.fn_span,
                "safe constructor returns a drop-critical type by ownership",
            )
            .with_note(format!(
                "return type `{}` is drop-critical (its `Drop` impl maintains an \
                 external invariant). Returning it by ownership from safe code lets \
                 callers `mem::forget` the value, skipping `Drop` and breaking the \
                 invariant — see RUSTSEC-2020-0021 (rio).",
                return_ty
            ))
            .with_help(
                "either mark this function `unsafe`, return the value behind a \
                 pinning wrapper that disallows safe ownership transfer, or \
                 redesign the type so its invariant does not depend on `Drop`",
            )
            .emit();
    }
}
