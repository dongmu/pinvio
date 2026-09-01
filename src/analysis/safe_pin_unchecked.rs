use super::util;
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_index::IndexVec;
use rustc_middle::mir::{
    Body, Local, Location, Operand, Rvalue, Statement, StatementKind, Terminator,
    TerminatorEdges, TerminatorKind,
};
use rustc_middle::ty::TyCtxt;
use rustc_mir_dataflow::fmt::DebugWithContext;
use rustc_mir_dataflow::{Analysis, JoinSemiLattice, ResultsVisitor, visit_reachable_results};
use rustc_span::Span;

pub fn check_crate(tcx: TyCtxt<'_>) {
    let detector = Detector { tcx };
    detector.scan_functions();
}

struct Detector<'tcx> {
    tcx: TyCtxt<'tcx>,
}

impl<'tcx> Detector<'tcx> {
    fn scan_functions(&self) {
        for local_def_id in self.tcx.hir_body_owners() {
            let def_kind = self.tcx.def_kind(local_def_id);
            if !matches!(
                def_kind,
                hir::def::DefKind::Fn | hir::def::DefKind::AssocFn
            ) {
                continue;
            }
            let def_id = local_def_id.to_def_id();
            if self.should_skip(def_id) {
                continue;
            }
            self.check_function(def_id);
        }
    }

    fn should_skip(&self, def_id: DefId) -> bool {
        let fn_sig = self.tcx.fn_sig(def_id).instantiate_identity().skip_binder();
        if fn_sig.safety == hir::Safety::Unsafe {
            return true;
        }
        def_id.as_local().is_none()
    }

    fn check_function(&self, fn_def_id: DefId) {
        let body = self.tcx.optimized_mir(fn_def_id);

        let analysis = ProvenanceAnalysis { tcx: self.tcx, body };
        let mut r = analysis.iterate_to_fixpoint(self.tcx, body, None);

        let mut visitor = CallSiteVisitor {
            tcx: self.tcx,
            reported_at: std::collections::HashSet::new(),
        };
        visit_reachable_results(body, &mut r.analysis, &r.results, &mut visitor);
    }
}

// =============================================================================
// Lattice
// =============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Provenance {
    Unknown,
    FromParam,
    FromOwnedLocal,
    Mixed,
}

impl Provenance {
    fn join(self, other: Provenance) -> Provenance {
        use Provenance::*;
        match (self, other) {
            (Unknown, x) | (x, Unknown) => x,
            (a, b) if a == b => a,
            _ => Mixed,
        }
    }

    fn is_suspicious(self) -> bool {
        matches!(self, Provenance::FromParam | Provenance::Mixed)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct ProvenanceMap(IndexVec<Local, Provenance>);

impl JoinSemiLattice for ProvenanceMap {
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

impl<C> DebugWithContext<C> for ProvenanceMap {}

// =============================================================================
// Dataflow analysis
// =============================================================================

struct ProvenanceAnalysis<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
}

impl<'a, 'tcx> Analysis<'tcx> for ProvenanceAnalysis<'a, 'tcx> {
    type Domain = ProvenanceMap;
    const NAME: &'static str = "safe_pin_unchecked_provenance";

    fn bottom_value(&self, body: &Body<'tcx>) -> Self::Domain {
        ProvenanceMap(IndexVec::from_elem_n(
            Provenance::Unknown,
            body.local_decls.len(),
        ))
    }

    fn initialize_start_block(&self, body: &Body<'tcx>, state: &mut Self::Domain) {
        for arg_local in body.args_iter() {
            let ty = body.local_decls[arg_local].ty;
            if util::is_reference_like(ty) {
                state.0[arg_local] = Provenance::FromParam;
            }
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
        state.0[lhs_local] = self.provenance_of_rvalue(rvalue, &state.0);
    }

    fn apply_primary_terminator_effect<'mir>(
        &mut self,
        state: &mut Self::Domain,
        terminator: &'mir Terminator<'tcx>,
        _location: Location,
    ) -> TerminatorEdges<'mir, 'tcx> {
        if let TerminatorKind::Call { destination, .. } = &terminator.kind {
            if let Some(dest_local) = destination.as_local() {
                state.0[dest_local] = Provenance::FromOwnedLocal;
            }
        }
        terminator.edges()
    }
}

impl<'a, 'tcx> ProvenanceAnalysis<'a, 'tcx> {
    fn provenance_of_rvalue(
        &self,
        rvalue: &Rvalue<'tcx>,
        state: &IndexVec<Local, Provenance>,
    ) -> Provenance {
        match rvalue {
            Rvalue::Ref(_, _, place) | Rvalue::RawPtr(_, place) => state[place.local],
            Rvalue::Use(operand) => self.provenance_of_operand(operand, state),
            Rvalue::Cast(_, operand, _) => self.provenance_of_operand(operand, state),
            Rvalue::Aggregate(_, _) => Provenance::FromOwnedLocal,
            _ => Provenance::FromOwnedLocal,
        }
    }

    fn provenance_of_operand(
        &self,
        operand: &Operand<'tcx>,
        state: &IndexVec<Local, Provenance>,
    ) -> Provenance {
        match operand {
            Operand::Move(place) | Operand::Copy(place) => state[place.local],
            Operand::Constant(_) => Provenance::FromOwnedLocal,
        }
    }
}

// =============================================================================
// Diagnostic emission
// =============================================================================

struct CallSiteVisitor<'tcx> {
    tcx: TyCtxt<'tcx>,
    reported_at: std::collections::HashSet<Span>,
}

impl<'a, 'tcx> ResultsVisitor<'tcx, ProvenanceAnalysis<'a, 'tcx>>
    for CallSiteVisitor<'tcx>
{
    fn visit_after_early_terminator_effect(
        &mut self,
        _analysis: &mut ProvenanceAnalysis<'a, 'tcx>,
        state: &ProvenanceMap,
        terminator: &Terminator<'tcx>,
        _location: Location,
    ) {
        let TerminatorKind::Call { func, args, .. } = &terminator.kind else { return };
        let Some(callee) = util::mir_callee_def_id(func) else { return };
        if !util::is_pin_new_unchecked(self.tcx, callee) {
            return;
        }
        let Some(arg) = args.first() else { return };

        let provenance = match &arg.node {
            Operand::Move(place) | Operand::Copy(place) => state.0[place.local],
            Operand::Constant(_) => return,
        };

        if !provenance.is_suspicious() {
            return;
        }

        let span = terminator.source_info.span;
        if !self.reported_at.insert(span) {
            return;
        }
        self.report(span, provenance);
    }
}

impl<'tcx> CallSiteVisitor<'tcx> {
    fn report(&self, span: Span, provenance: Provenance) {
        let provenance_note = match provenance {
            Provenance::FromParam => "argument provenance traces back to a function parameter",
            Provenance::Mixed => {
                "argument provenance is `Mixed` — at least one control-flow path \
                 reaches here from a function parameter"
            }
            _ => return,
        };
        self.tcx
            .dcx()
            .struct_span_warn(
                span,
                "`Pin::new_unchecked` is called on memory whose pinning invariant \
                 is not enforced by this function's signature",
            )
            .with_note(provenance_note)
            .with_note(
                "callers of this safe function never opted into the pinning \
                 contract — they passed an ordinary `&mut T` and received back \
                 a `Pin<&mut T>` that depends on storage stability",
            )
            .with_help(
                "either mark this function `unsafe` and document the pinning \
                 precondition, or change the receiver to `Pin<&mut Self>` \
                 (or take the value by ownership in a Box) so that the caller \
                 must establish the pinning invariant before calling",
            )
            .emit();
    }
}
