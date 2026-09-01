#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_ast_pretty;
extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;

use rustc_driver::Compilation;
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use pinvio::analyze;
use pinvio::llog::Verbosity;
use pinvio::{p_error, p_info};

struct PinvioCompilerCalls;

impl PinvioCompilerCalls {
    pub fn new() -> PinvioCompilerCalls {
        PinvioCompilerCalls {}
    }
}

impl rustc_driver::Callbacks for PinvioCompilerCalls {
    fn after_analysis<'tcx>(&mut self, compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        compiler.sess.dcx().abort_if_errors();

        p_info!("====================");
        p_info!(
            "Crate: {} (entry point: {})",
            compiler
                .sess
                .opts
                .crate_name
                .clone()
                .unwrap_or_else(|| "unknown-crate".to_string()),
            compiler.sess.io.input.source_name().prefer_local()
        );
        p_info!("Note: analysis covers ALL modules in the crate, not only the entry point file.");

        analyze(tcx);

        compiler.sess.dcx().abort_if_errors();
        Compilation::Stop
    }
}

fn run_compiler(args: &[String], callbacks: &mut (dyn rustc_driver::Callbacks + Send)) -> ! {
    let exit_code =
        rustc_driver::catch_with_exit_code(move || rustc_driver::run_compiler(args, callbacks));
    std::process::exit(exit_code)
}

fn parse_config() -> Vec<String> {
    let mut iargs = vec![];
    for arg in std::env::args() {
        match arg.as_str() {
            "-Zpinvio" => {
                p_error!("TODO - Getting args!");
            }
            _ => {
                iargs.push(arg);
            }
        }
    }

    iargs
}

pub fn compile_time_sysroot() -> Option<String> {
    Some(
        option_env!("RUST_SYSROOT")
            .expect("Set the `RUST_SYSROOT` env var at build time")
            .to_owned(),
    )
}

fn main() {
    rustc_driver::install_ice_hook("No URL to report", |_| ());

    let mut iargs = parse_config();
    pinvio::llog::setup_logging(Verbosity::Verbose).expect("Initalize Log!");

    if let Some(sysroot) = compile_time_sysroot() {
        let sysroot_flag = "--sysroot";
        if !iargs.iter().any(|e| e == sysroot_flag) {
            iargs.push(sysroot_flag.to_owned());
            iargs.push(sysroot);
        }
    }

    run_compiler(iargs.as_ref(), &mut PinvioCompilerCalls::new())
}
