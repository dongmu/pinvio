use super::util;
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_middle::hir::nested_filter;
use rustc_middle::mir::{self, Body, Rvalue, StatementKind, TerminatorKind};
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::Span;
use std::collections::HashSet;

pub fn check_crate(tcx: TyCtxt<'_>) {
    let detector = Detector::new(tcx);
    let candidates = detector.find_candidate_types();
    for (impl_def_id, target_method, span) in candidates {
        detector.inspect_deref_body(impl_def_id, target_method, span);
    }
}

struct Detector<'tcx> {
    tcx: TyCtxt<'tcx>,
}

#[derive(Clone, Copy)]
enum DerefMethod {
    Deref,
    DerefMut,
}

impl<'tcx> Detector<'tcx> {
    fn new(tcx: TyCtxt<'tcx>) -> Self {
        Self { tcx }
    }

    fn find_candidate_types(&self) -> Vec<(DefId, DerefMethod, Span)> {
        let mut pinned_pointer_types: HashSet<DefId> = HashSet::new();
        let mut pin_visitor = PinUsageFinder {
            tcx: self.tcx,
            pinned_pointer_types: &mut pinned_pointer_types,
        };
        self.tcx.hir_visit_all_item_likes_in_crate(&mut pin_visitor);

        let mut candidates = Vec::new();
        let deref_trait = self.tcx.lang_items().deref_trait();
        let deref_mut_trait = self.tcx.lang_items().deref_mut_trait();

        for item_id in util::all_items(self.tcx) {
            let item = self.tcx.hir_item(item_id);
            let hir::ItemKind::Impl(impl_data) = item.kind else { continue };
            let Some(of_trait) = impl_data.of_trait else { continue };
            let Some(trait_def_id) = of_trait.trait_def_id() else { continue };

            let method = if Some(trait_def_id) == deref_trait {
                DerefMethod::Deref
            } else if Some(trait_def_id) == deref_mut_trait {
                DerefMethod::DerefMut
            } else {
                continue;
            };

            let self_ty = self.tcx.type_of(item.owner_id.def_id).instantiate_identity();
            let ty::Adt(adt_def, _) = self_ty.kind() else { continue };
            if !pinned_pointer_types.contains(&adt_def.did()) {
                continue;
            }

            candidates.push((item.owner_id.to_def_id(), method, item.span));
        }
        candidates
    }

    fn inspect_deref_body(&self, impl_def_id: DefId, method: DerefMethod, impl_span: Span) {
        let assoc_items = self.tcx.associated_items(impl_def_id);
        let target_name = match method {
            DerefMethod::Deref => "deref",
            DerefMethod::DerefMut => "deref_mut",
        };
        let Some(assoc) = assoc_items
            .in_definition_order()
            .find(|a| a.name().as_str() == target_name)
        else {
            return;
        };
        let method_def_id = assoc.def_id;
        let Some(local_def_id) = method_def_id.as_local() else { return };

        let body = self.tcx.optimized_mir(method_def_id);

        let mut findings = Vec::new();
        self.scan_for_allocation(body, &mut findings);
        self.scan_for_branching_returns(body, &mut findings);
        self.scan_for_mem_replace_swap(body, &mut findings);

        if !findings.is_empty() {
            self.report(impl_span, target_name, &findings);
        }
        let _ = local_def_id;
    }

    fn scan_for_allocation(&self, body: &Body<'_>, findings: &mut Vec<Finding>) {
        for (bb, data) in body.basic_blocks.iter_enumerated() {
            let Some(term) = &data.terminator else { continue };
            let TerminatorKind::Call { func, .. } = &term.kind else { continue };
            let Some(callee_did) = util::mir_callee_def_id(func) else { continue };
            let path = self.tcx.def_path_str(callee_did);

            const ALLOC_FUNCS: &[&str] = &[
                "alloc::boxed::Box::<T>::new",
                "alloc::alloc::alloc",
                "alloc::vec::Vec::<T>::with_capacity",
                "alloc::vec::Vec::<T>::new",
                "alloc::sync::Arc::<T>::new",
                "alloc::rc::Rc::<T>::new",
            ];
            for needle in ALLOC_FUNCS {
                if path.contains(needle) || path.ends_with(&format!("::{needle}")) {
                    findings.push(Finding {
                        span: term.source_info.span,
                        kind: FindingKind::Allocation(path.clone()),
                    });
                    let _ = bb;
                    break;
                }
            }
        }
    }

    fn scan_for_branching_returns(&self, body: &Body<'_>, findings: &mut Vec<Finding>) {
        let return_local = mir::Local::from_u32(0);
        let mut return_sources: HashSet<String> = HashSet::new();

        for data in body.basic_blocks.iter() {
            for stmt in &data.statements {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else { continue };
                if place.local != return_local {
                    continue;
                }
                let key = match rvalue {
                    Rvalue::Ref(_, _, inner_place) | Rvalue::RawPtr(_, inner_place) => {
                        format!("{inner_place:?}")
                    }
                    Rvalue::Use(op) => format!("{op:?}"),
                    _ => continue,
                };
                return_sources.insert(key);
            }
        }

        if return_sources.len() > 1 {
            findings.push(Finding {
                span: body.span,
                kind: FindingKind::BranchingReturn(return_sources.len()),
            });
        }
    }

    fn scan_for_mem_replace_swap(&self, body: &Body<'_>, findings: &mut Vec<Finding>) {
        for data in body.basic_blocks.iter() {
            let Some(term) = &data.terminator else { continue };
            let TerminatorKind::Call { func, .. } = &term.kind else { continue };
            let Some(callee_did) = util::mir_callee_def_id(func) else { continue };
            let path = self.tcx.def_path_str(callee_did);

            if path.ends_with("mem::replace")
                || path.ends_with("mem::swap")
                || path.ends_with("ptr::write")
                || path.ends_with("ptr::read")
            {
                findings.push(Finding {
                    span: term.source_info.span,
                    kind: FindingKind::MemMutation(path),
                });
            }
        }
    }

    fn report(&self, span: Span, method_name: &str, findings: &[Finding]) {
        let mut diag = self.tcx.dcx().struct_span_warn(
            span,
            format!(
                "`{method_name}` impl on a type used in `Pin<...>` may violate Pin's \
                 location-stability guarantee"
            ),
        );
        for finding in findings {
            match &finding.kind {
                FindingKind::Allocation(path) => {
                    diag = diag.with_span_note(
                        finding.span,
                        format!("allocation in `{method_name}` body via `{path}`"),
                    );
                }
                FindingKind::BranchingReturn(n) => {
                    diag = diag.with_note(format!(
                        "{n} distinct expressions flow into the return value — \
                         a sound `{method_name}` impl returns one fixed field address"
                    ));
                }
                FindingKind::MemMutation(path) => {
                    diag = diag.with_span_note(
                        finding.span,
                        format!("call to `{path}` mutates the receiver"),
                    );
                }
            }
        }
        diag.with_help(
            "ensure `deref`/`deref_mut` returns the same address on every call. \
             Pin<P> relies on this for location stability of `!Unpin` targets.",
        )
        .emit();
    }
}

struct Finding {
    span: Span,
    kind: FindingKind,
}

enum FindingKind {
    Allocation(String),
    BranchingReturn(usize),
    MemMutation(String),
}

struct PinUsageFinder<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    pinned_pointer_types: &'a mut HashSet<DefId>,
}

impl<'a, 'tcx> Visitor<'tcx> for PinUsageFinder<'a, 'tcx> {
    type NestedFilter = nested_filter::All;

    fn maybe_tcx(&mut self) -> Self::MaybeTyCtxt {
        self.tcx
    }

    fn visit_ty(&mut self, ty: &'tcx hir::Ty<'tcx, hir::AmbigArg>) {
        if let hir::TyKind::Path(hir::QPath::Resolved(_, path)) = &ty.kind {
            if let Some(def_id) = path.res.opt_def_id() {
                if util::matches_path(self.tcx, def_id, &["core", "pin", "Pin"])
                    || util::matches_path(self.tcx, def_id, &["std", "pin", "Pin"])
                {
                    if let Some(seg) = path.segments.last() {
                        if let Some(args) = seg.args {
                            for arg in args.args {
                                if let hir::GenericArg::Type(inner_ty) = arg {
                                    self.record_pointer_type((*inner_ty).as_unambig_ty());
                                }
                            }
                        }
                    }
                }
            }
        }
        intravisit::walk_ty(self, ty);
    }
}

impl<'a, 'tcx> PinUsageFinder<'a, 'tcx> {
    fn record_pointer_type(&mut self, ty: &hir::Ty<'_>) {
        let hir_ty = match &ty.kind {
            hir::TyKind::Ref(_, mut_ty) => mut_ty.ty,
            _ => ty,
        };
        if let hir::TyKind::Path(hir::QPath::Resolved(_, path)) = &hir_ty.kind {
            if let Some(def_id) = path.res.opt_def_id() {
                self.pinned_pointer_types.insert(def_id);
            }
        }
    }
}
