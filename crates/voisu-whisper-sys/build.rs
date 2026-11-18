//! Builds the vendored whisper.cpp (see `vendor.sh`) and the C shim.
//!
//! Phase 1 is CPU only. A GPU backend later is one more `GGML_<NAME>` define
//! here plus its static library in `LIBS` and its system library link.

use std::env;
use std::path::PathBuf;

/// Link order matters for static archives: users before what they use.
const LIBS: &[&str] = &[
    "voisu_whisper_shim",
    "whisper",
    "ggml",
    "ggml-cpu",
    "ggml-base",
];

fn main() {
    println!("cargo:rerun-if-changed=CMakeLists.txt");
    println!("cargo:rerun-if-changed=shim.c");
    println!("cargo:rerun-if-changed=whisper.cpp");

    let out = cmake::Config::new(".")
        // A Debug ggml decodes many times slower; the worker is never useful so.
        .profile("Release")
        .define("GGML_CPU", "ON")
        .build();

    for dir in ["lib", "lib64"] {
        let path: PathBuf = out.join(dir);
        if path.is_dir() {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    }
    for lib in LIBS {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    match (target_os.as_str(), target_env.as_str()) {
        ("macos" | "ios", _) => println!("cargo:rustc-link-lib=c++"),
        (_, "msvc") => {}
        _ => println!("cargo:rustc-link-lib=stdc++"),
    }
}
