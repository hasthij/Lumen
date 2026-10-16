use std::path::{Path, PathBuf};
use std::process::Command;

/// Newest Windows SDK resource compiler, if one is installed.
fn find_rc() -> Option<PathBuf> {
    let bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin).ok()?.flatten().map(|e| e.path()).filter(|p| p.join("x64").join("rc.exe").exists()).collect();
    versions.sort();
    versions.pop().map(|v| v.join("x64").join("rc.exe"))
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("res");
    let manifest = dir.join("lumen.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", dir.join("lumen.rc").display());
    println!("cargo:rerun-if-changed={}", dir.join("lumen.ico").display());
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());

    // Embed the application icon (shown on the taskbar, in Explorer and in the Start menu).
    match find_rc() {
        Some(rc) => {
            let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("lumen.res");
            let ok = Command::new(rc).current_dir(&dir).arg("/nologo").arg("/fo").arg(&out).arg("lumen.rc").status().map(|s| s.success()).unwrap_or(false);
            if ok {
                println!("cargo:rustc-link-arg-bins={}", out.display());
            } else {
                println!("cargo:warning=could not compile the icon resource");
            }
        }
        None => println!("cargo:warning=Windows SDK resource compiler not found; building without an exe icon"),
    }
}
