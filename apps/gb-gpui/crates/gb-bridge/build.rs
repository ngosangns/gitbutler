use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=askpass/main.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("askpass/main.rs");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("gitbutler-git-askpass");
    compile_helper(&source, &dest);
    println!("cargo:rustc-env=GB_ASKPASS_HELPER={}", dest.display());
    if let Some(profile) = profile_dir(&out_dir) {
        let sibling = profile.join(helper_file_name());
        if let Err(error) = std::fs::copy(&dest, &sibling) {
            println!("cargo:warning=could not copy the askpass helper beside gb-gpui: {error}");
        }
    }
}

fn compile_helper(source: &Path, dest: &Path) {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let mut command = Command::new(&rustc);
    command
        .arg("--edition=2021")
        .arg("-C")
        .arg("opt-level=3")
        .arg("-C")
        .arg("strip=symbols")
        .arg("--crate-name")
        .arg("gitbutler_git_askpass")
        .arg(source)
        .arg("-o")
        .arg(dest);
    let target = std::env::var("TARGET").ok();
    let host = std::env::var("HOST").ok();
    if let Some(target) = target.filter(|target| host.as_ref() != Some(target)) {
        command.arg("--target").arg(target);
    }
    let status = command.status().unwrap_or_else(|error| {
        panic!("could not run {rustc} for the askpass helper: {error}");
    });
    if !status.success() {
        panic!("the askpass helper failed to compile");
    }
}

/// `<target>/<profile>/build/<pkg>/out` → `<target>/<profile>`.
fn profile_dir(out_dir: &Path) -> Option<PathBuf> {
    let build = out_dir.parent()?.parent()?;
    if build.file_name()? != "build" {
        return None;
    }
    Some(build.parent()?.to_path_buf())
}

fn helper_file_name() -> String {
    format!("gitbutler-git-askpass{}", std::env::consts::EXE_SUFFIX)
}
