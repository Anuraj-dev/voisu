//! Builds the vendored whisper.cpp (see `vendor.sh`) and the C shim.
//!
//! # Backends
//!
//! ggml is built with `GGML_BACKEND_DL=ON`: `libwhisper.so.1`,
//! `libggml.so.0` and `libggml-base.so.0` are linked, and every backend is a
//! module the worker loads at start from its own directory
//! (`voisu_whisper_load_backends`):
//!
//! - CPU: `GGML_CPU_ALL_VARIANTS=ON` builds `libggml-cpu-<variant>.so` from
//!   plain `x64` up to `sapphirerapids`. ggml scores each against the running
//!   CPU and keeps the best, so no single ISA baseline is pinned and no
//!   variant runs code the CPU lacks. `GGML_NATIVE` stays off: on a build
//!   host `-march=native` would leak the build CPU into a shipped module.
//! - Vulkan: the `vulkan` feature adds `libggml-vulkan.so`. It needs `glslc`
//!   and the Vulkan headers and loader at build time (Fedora:
//!   `vulkan-loader-devel glslc`; Arch: `vulkan-headers shaderc`).
//!
//! The runtime files are copied next to the cargo binaries and test binaries
//! (`target/<profile>/` and its `deps/`), and their names are listed in
//! `target/<profile>/voisu-whisper-runtime.txt`, which packaging installs
//! from. Binaries find the libraries through an `$ORIGIN` rpath (emitted here
//! for this crate's tests and by `voisu-local-asr/build.rs` for the worker);
//! packages install the same files beside `voisu-local-asr` in
//! `/usr/libexec/voisu/`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The linked libraries, under the SONAME the binary records.
const LINKED: &[&str] = &["libwhisper.so.1", "libggml.so.0", "libggml-base.so.0"];

fn main() {
    println!("cargo:rerun-if-changed=CMakeLists.txt");
    println!("cargo:rerun-if-changed=shim.c");
    println!("cargo:rerun-if-changed=whisper.cpp");

    let vulkan = env::var_os("CARGO_FEATURE_VULKAN").is_some();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let mut config = cmake::Config::new(".");
    // A Debug ggml decodes many times slower; the worker is never useful so.
    config
        .profile("Release")
        .define("GGML_BACKEND_DL", "ON")
        .define("GGML_CPU", "ON")
        .define("GGML_NATIVE", "OFF")
        .define("GGML_VULKAN", if vulkan { "ON" } else { "OFF" });
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        config.define("GGML_CPU_ALL_VARIANTS", "ON");
    }
    let out = config.build();

    let mut runtime = Vec::new();
    for dir in ["lib", "lib64"] {
        let path: PathBuf = out.join(dir);
        if path.is_dir() {
            println!("cargo:rustc-link-search=native={}", path.display());
            runtime.extend(LINKED.iter().map(|name| path.join(name)));
        }
    }
    // Backend modules install to bin/ (ggml's CMAKE_INSTALL_BINDIR default).
    runtime.extend(modules(&out.join("bin")));
    let runtime: Vec<PathBuf> = runtime.into_iter().filter(|path| path.exists()).collect();
    assert!(
        runtime
            .iter()
            .any(|path| file_name(path).starts_with("libggml-cpu")),
        "no CPU backend module was built"
    );
    if vulkan {
        assert!(
            runtime
                .iter()
                .any(|path| file_name(path) == "libggml-vulkan.so"),
            "the vulkan feature built no libggml-vulkan.so"
        );
    }

    println!("cargo:rustc-link-lib=static=voisu_whisper_shim");
    for lib in ["whisper", "ggml", "ggml-base"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    match (target_os.as_str(), target_env.as_str()) {
        ("macos" | "ios", _) => println!("cargo:rustc-link-lib=c++"),
        (_, "msvc") => {}
        _ => println!("cargo:rustc-link-lib=stdc++"),
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");

    // OUT_DIR is <profile>/build/<crate>-<hash>/out, or
    // <profile>/build/<crate>/<hash>/out under cargo's newer build-dir
    // layout (the default from Rust 1.100). Both sit under `build/`.
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let profile = out_dir
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
        .expect("cargo profile dir");
    // Unit tests read this: their binaries no longer sit in the profile dir.
    println!(
        "cargo:rustc-env=VOISU_WHISPER_RUNTIME_DIR={}",
        profile.display()
    );
    for dest in [profile.to_path_buf(), profile.join("deps")] {
        fs::create_dir_all(&dest).expect("create runtime dir");
        for source in &runtime {
            copy_if_changed(source, &dest.join(file_name(source)));
        }
    }
    let names: Vec<&str> = runtime.iter().map(|path| file_name(path)).collect();
    fs::write(
        profile.join("voisu-whisper-runtime.txt"),
        names.join("\n") + "\n",
    )
    .expect("write runtime list");
}

fn modules(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = file_name(path);
            name.starts_with("libggml-") && name.ends_with(".so")
        })
        .collect();
    found.sort();
    found
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 file name")
}

/// Follows symlinks (the SONAME links point at the versioned files) and
/// skips identical copies so a no-op rebuild does not touch the targets.
fn copy_if_changed(source: &Path, dest: &Path) {
    let bytes = fs::read(source).expect("read runtime file");
    if fs::read(dest).ok().as_deref() == Some(bytes.as_slice()) {
        return;
    }
    let partial = dest.with_extension("partial");
    fs::write(&partial, &bytes).expect("write runtime file");
    fs::rename(&partial, dest).expect("publish runtime file");
}
