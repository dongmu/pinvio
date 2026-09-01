#![feature(rustc_private)]
#![feature(box_patterns)]

extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_index;
extern crate rustc_infer;
extern crate rustc_middle;
extern crate rustc_mir_dataflow;
extern crate rustc_span;
extern crate rustc_trait_selection;

pub mod analysis;
pub mod llog;

use rustc_middle::ty::TyCtxt;

fn run_analysis<F, R>(name: &str, f: F) -> R
where
    F: FnOnce() -> R,
{
    p_info!("++++++++++++++++++++ Running: {}", name);
    let result = f();
    p_info!("-------------------- Complete: {}", name);
    result
}

pub fn analyze<'tcx>(tcx: TyCtxt<'tcx>) {
    let source_map = tcx.sess.source_map();
    let mut source_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item_id in analysis::util::all_items(tcx) {
        let item = tcx.hir_item(item_id);
        let filename = format!("{}", source_map.span_to_filename(item.span).prefer_local());
        // Only count real on-disk files (skip macro expansion spans etc.)
        if !filename.starts_with('<') {
            source_files.insert(filename);
        }
    }
    p_info!(
        "Scope: {} source file(s) in crate — {}",
        source_files.len(),
        {
            let mut v: Vec<&str> = source_files.iter().map(|s| s.as_str()).collect();
            v.sort();
            v.join(", ")
        }
    );

    run_analysis("MissingUnpinBound", || {
        analysis::missing_unpin_bound::check_crate(tcx);
    });
    run_analysis("WrongUnpinImpl", || {
        analysis::wrong_unpin_impl::check_crate(tcx);
    });
    run_analysis("PinLeakDetector", || {
        analysis::pin_leak_detector::check_crate(tcx);
    });
    run_analysis("DerefPinStability", || {
        analysis::deref_pin_stability::check_crate(tcx);
    });
    run_analysis("SafePinUnchecked", || {
        analysis::safe_pin_unchecked::check_crate(tcx);
    });
}
