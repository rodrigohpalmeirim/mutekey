//! Embeds resources.rc (the icons) into the executable. Uses MinGW's windres, which is what
//! the x86_64-pc-windows-gnu target links with.
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=resources.rc");
    println!("cargo:rerun-if-changed=icons");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let windres = if cfg!(windows) {
        "windres".to_string()
    } else {
        format!("{}-w64-mingw32-windres", env::var("CARGO_CFG_TARGET_ARCH").unwrap())
    };
    let object = PathBuf::from(env::var("OUT_DIR").unwrap()).join("resources.o");
    let status = Command::new(&windres)
        .args(["resources.rc", "-O", "coff", "-o"])
        .arg(&object)
        .status()
        .unwrap_or_else(|e| panic!("couldn't run {windres}: {e}"));
    assert!(status.success(), "{windres} failed");
    println!("cargo:rustc-link-arg-bins={}", object.display());
}
