#![feature(rustc_private)]

///! This implementation is based on "cargo-rudra"
use std::env;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rustc_version::VersionMeta;

use wait_timeout::ChildExt;

use pinvio::{p_info, p_error, p_warn};

/// Gets the value of a `--flag`.
fn get_arg_flag_value(name: &str) -> Option<String> {
    // Stop searching at `--`.
    let mut args = std::env::args().take_while(|val| val != "--");
    loop {
        let arg = match args.next() {
            Some(arg) => arg,
            None => return None,
        };
        if !arg.starts_with(name) {
            continue;
        }
        // Strip leading `name`.
        let suffix = &arg[name.len()..];
        if suffix.is_empty() {
            // This argument is exactly `name`; the next one is the value.
            return args.next();
        } else if suffix.starts_with('=') {
            // This argument is `name=value`; get the value.
            // Strip leading `=`.
            return Some(suffix[1..].to_owned());
        }
    }
}

fn any_arg_flag<F>(name: &str, mut check: F) -> bool
where
    F: FnMut(&str) -> bool,
{
    // Stop searching at `--`.
    let mut args = std::env::args().take_while(|val| val != "--");
    loop {
        let arg = match args.next() {
            Some(arg) => arg,
            None => return false,
        };
        if !arg.starts_with(name) {
            continue;
        }

        // Strip leading `name`.
        let suffix = &arg[name.len()..];
        let value = if suffix.is_empty() {
            // This argument is exactly `name`; the next one is the value.
            match args.next() {
                Some(arg) => arg,
                None => return false,
            }
        } else if suffix.starts_with('=') {
            // This argument is `name=value`; get the value.
            // Strip leading `=`.
            suffix[1..].to_owned()
        } else {
            return false;
        };

        if check(&value) {
            return true;
        }
    }
}

fn show_error(msg: impl AsRef<str>) -> ! {
    p_error!("{}", msg.as_ref());
    std::process::exit(1)
}

fn clean_package(package_name: &str) {
    let mut cmd = Command::new("cargo");
    cmd.arg("clean");

    cmd.arg("-p");
    cmd.arg(package_name);

    cmd.arg("--target");
    cmd.arg(version_info().host);

    let exit_status = cmd
        .spawn()
        .expect("could not run cargo clean")
        .wait()
        .expect("failed to wait for cargo?");

    if !exit_status.success() {
        show_error(format!("cargo clean failed"));
    }
}

fn find_pinvio() -> PathBuf {
    let mut path = std::env::current_exe().expect("current executable path invalid");
    path.set_file_name("pinvio");
    path
}

fn get_first_arg_with_rs_suffix() -> Option<String> {
    // Stop searching at `--`.
    let mut args = std::env::args().take_while(|val| val != "--");
    args.find(|arg| arg.ends_with(".rs"))
}

fn version_info() -> VersionMeta {
    VersionMeta::for_command(Command::new(find_pinvio()))
        .expect("failed to determine underlying rustc version of pinvio")
}

fn cargo_package() -> cargo_metadata::Package {
    let manifest_path =
        get_arg_flag_value("--manifest-path").map(|m| Path::new(&m).canonicalize().unwrap());

    let mut cmd = cargo_metadata::MetadataCommand::new();
    if let Some(manifest_path) = &manifest_path {
        cmd.manifest_path(manifest_path);
    }
    let mut metadata = match cmd.exec() {
        Ok(metadata) => metadata,
        Err(e) => show_error(format!("could not obtain Cargo metadata!\n{}", e)),
    };

    let current_dir = std::env::current_dir();

    let package_index = metadata
        .packages
        .iter()
        .position(|package| {
            let package_manifest_path = Path::new(&package.manifest_path);
            if let Some(manifest_path) = &manifest_path {
                package_manifest_path == manifest_path
            } else {
                let current_dir = current_dir
                    .as_ref()
                    .expect("could not read current directory");
                let package_manifest_directory = package_manifest_path
                    .parent()
                    .expect("could not find parent folder of package manifest");
                package_manifest_directory == current_dir
            }
        })
        .unwrap_or_else(|| {
            show_error("This seems to be a workspace, which is not supported by pinvio.")
        });

    metadata.packages.remove(package_index)
}

fn main() {
    let pinvio_or_rustc = std::env::args().nth(1).expect("Get the second arg");

    if pinvio_or_rustc.ends_with("pinvio") {
        p_info!("==> Running cargo pinvio");
        into_cargo_pinvio();
        p_info!("==> cargo pinvio finished");
    } else if pinvio_or_rustc.ends_with("rustc") {
        // The args has the full path including `rustc`, but not exactly `rustc`.
        into_cargo_rustc();
    } else {
        p_error!("`cargo-pinvio` must be called with either `pinvio` or `rustc` as first argument.");
    }
}

#[repr(u8)]
enum TargetKind {
    Library = 0,
    Bin,
    Unknown,
}

impl TargetKind {
    fn is_lib_str(s: &str) -> bool {
        s == "lib" || s == "rlib" || s == "staticlib"
    }
}

impl From<&cargo_metadata::Target> for TargetKind {
    fn from(target: &cargo_metadata::Target) -> Self {
        if target
            .kind
            .iter()
            .any(|s| TargetKind::is_lib_str(&s.to_string()))
        {
            TargetKind::Library
        } else if let Some(&cargo_metadata::TargetKind::Bin) = target.kind.get(0) {
            TargetKind::Bin
        } else {
            TargetKind::Unknown
        }
    }
}

impl Display for TargetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                TargetKind::Library => "lib",
                TargetKind::Bin => "bin",
                TargetKind::Unknown => "unknown",
            }
        )
    }
}

fn into_cargo_pinvio() {
    // test_sysroot_consistency();

    let package = cargo_package();
    let mut targets: Vec<_> = package.targets.into_iter().collect();

    // Ensure `lib` is compiled before `bin`
    targets.sort_by_key(|target| TargetKind::from(target) as u8);

    for target in targets {
        p_info!("x-x-x-x-x-x-x-x-x-x-x");
        p_info!("+++ {:?}", target);

        let mut args = std::env::args().skip(2);
        let kind = TargetKind::from(&target);

        let mut cmd = Command::new("cargo");
        cmd.arg("check");

        match kind {
            TargetKind::Bin => {
                cmd.arg("--bin").arg(&target.name);
            }
            TargetKind::Library => {
                cmd.arg("--lib");
                clean_package(&package.name);
            }
            TargetKind::Unknown => {
                p_error!(
                    "Target {:?}:{} is not supported",
                    target.kind.as_slice(),
                    &target.name
                );
                continue;
            }
        }

        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
            cmd.arg(arg);
        }

        if get_arg_flag_value("--target").is_none() {
            cmd.arg("--target");
            cmd.arg(version_info().host);
        }

        let args_vec: Vec<String> = args.collect();
        cmd.env(
            "PINVIO_ARGS",
            serde_json::to_string(&args_vec).expect("failed to serialize args"),
        );

        if env::var_os("RUSTC_WRAPPER").is_some() {
            p_warn!("WARNING: Ignoring existing `RUSTC_WRAPPER` environment variable, pinvio does not support wrapping.");
        }

        let path = std::env::current_exe().expect("current executable path invalid");
        cmd.env("RUSTC_WRAPPER", path);

        p_info!("==> Running pinvio for target {}:{}", kind, &target.name);
        let mut child = cmd.spawn().expect("could not run cargo check");
        match child
            .wait_timeout(Duration::from_secs(60))
            .expect("failed to wait for subprocess")
        {
            Some(exit_status) => {
                if !exit_status.success() {
                    show_error("Finished with non-zero exit code");
                }
            }
            None => {
                child.kill().expect("failed to kill subprocess");
                child.wait().expect("failed to wait for subprocess");
                show_error("Killed due to timeout");
            }
        };
    }
}

fn into_cargo_rustc() {

    fn contains_target_flag() -> bool {
        get_arg_flag_value("--target").is_some()
    }

    fn is_target_crate() -> bool {
        let entry_path_arg = match get_first_arg_with_rs_suffix() {
            Some(arg) => arg,
            None => return false,
        };
        let entry_path: &Path = entry_path_arg.as_ref();

        entry_path.is_relative()
    }

    fn is_crate_type_lib() -> bool {
        any_arg_flag("--crate-type", TargetKind::is_lib_str)
    }

    fn run_command(mut cmd: Command) {
        match cmd.status() {
            Ok(exit) => {
                if !exit.success() {
                    std::process::exit(exit.code().unwrap_or(42));
                }
            }
            Err(e) => panic!("error running {:?}:\n{:?}", cmd, e),
        }
    }

    let is_direct_target = contains_target_flag() && is_target_crate();
    let is_additional_target = false;

    // Perform analysis if xxx

    if is_direct_target || is_additional_target {
        let mut cmd = Command::new(find_pinvio());
        cmd.args(std::env::args().skip(2));

        let magic = std::env::var("PINVIO_ARGS").expect("missing xx args");
        let pinvio_args: Vec<String> =
            serde_json::from_str(&magic).expect("failed to deserialize args");
        cmd.args(pinvio_args);

        run_command(cmd);
    }

    if !is_direct_target || is_crate_type_lib() {
        let mut cmd = Command::new("rustc");
        cmd.args(std::env::args().skip(2));

        run_command(cmd);
    }
}
