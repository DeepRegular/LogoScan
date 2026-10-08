//! Puts the icon into the Windows programs. The resource compiler is rc on
//! Windows and llvm-rc elsewhere (cargo xwin); without one the programs are
//! built without the icon.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets").join("icon.ico");
    let rc = out.join("icon.rc");
    let res = out.join("icon.res");
    let ico = ico.to_string_lossy().replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{ico}\"\n")).unwrap();
    let tools: &[&str] = if cfg!(windows) { &["rc", "llvm-rc"] } else { &["llvm-rc", "llvm-rc-19", "llvm-rc-18"] };
    for tool in tools {
        let ok = Command::new(tool).arg("/nologo").arg("/fo").arg(&res).arg(&rc).status().is_ok_and(|s| s.success());
        if ok {
            println!("cargo:rustc-link-arg-bins={}", res.display());
            return;
        }
    }
    println!("cargo:warning=no resource compiler (rc or llvm-rc): the programs are built without the icon");
}
