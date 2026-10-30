//! Embeds resources.rc (icons and version info) into the executable. Uses MinGW's windres,
//! which is what the x86_64-pc-windows-gnu target links with.
use std::{env, fs, path::PathBuf, process::Command};

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
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    // The version comes from Cargo.toml. A header rather than -D flags, because windres
    // hands defines to the preprocessor through a shell, which mangles quoted strings.
    let version = |part: &str| env::var(format!("CARGO_PKG_VERSION_{part}")).unwrap();
    let header = format!(
        "#define VERSION_NUMBER {},{},{},0\n#define VERSION_STRING \"{}\"\n",
        version("MAJOR"), version("MINOR"), version("PATCH"), env::var("CARGO_PKG_VERSION").unwrap(),
    );
    fs::write(out.join("version.h"), header).unwrap();
    let object = out.join("resources.o");
    let status = Command::new(&windres)
        .args(["resources.rc", "-O", "coff", "--include-dir"])
        .arg(&out)
        .arg("-o")
        .arg(&object)
        .status()
        .unwrap_or_else(|e| panic!("couldn't run {windres}: {e}"));
    assert!(status.success(), "{windres} failed");
    println!("cargo:rustc-link-arg-bins={}", object.display());
}
