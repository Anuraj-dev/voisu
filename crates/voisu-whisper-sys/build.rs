//! Builds the vendored whisper.cpp (see `vendor.sh`) and the C shim.
//!
//! Phase 1 is CPU only. A GPU backend later is one more `GGML_<NAME>` define
//! here plus its static library in `LIBS` and its system library link.
//!
//! # CPU instruction baseline
//!
//! ggml defaults to `GGML_NATIVE=ON` (`-march=native`: the binary dies of
//! SIGILL on any older CPU than the build host) unless `SOURCE_DATE_EPOCH` is
//! set, as under rpmbuild, where it silently drops to no SIMD at all. Neither
//! is acceptable for a shipped binary, so the ISA is pinned here:
//!
//! - x86_64: the x86-64-v3 level (Haswell / Zen 1 and later): SSE4.2, AVX,
//!   AVX2, FMA, F16C and BMI2 on; every AVX-512 flavour, AVX-VNNI and AMX
//!   off. `REQUIRED_CPU_FEATURES` in `src/lib.rs` lists the same set, and
//!   the worker refuses Prepare with `cpu_unsupported` on a CPU without it
//!   instead of crashing. Keep the two lists in step.
//! - aarch64 and others: ggml's own defaults with `GGML_NATIVE` off.
//!
//! Phase 2 packaging replaces the single baseline with `GGML_BACKEND_DL` and
//! `GGML_CPU_ALL_VARIANTS`, so ggml picks the best CPU kernel at run time.

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

/// x86-64-v3, matching `REQUIRED_CPU_FEATURES`.
const X86_64_ON: &[&str] = &[
    "GGML_SSE42",
    "GGML_AVX",
    "GGML_AVX2",
    "GGML_FMA",
    "GGML_F16C",
    "GGML_BMI2",
];
const X86_64_OFF: &[&str] = &[
    "GGML_AVX_VNNI",
    "GGML_AVX512",
    "GGML_AVX512_VBMI",
    "GGML_AVX512_VNNI",
    "GGML_AVX512_BF16",
    "GGML_AMX_TILE",
    "GGML_AMX_INT8",
    "GGML_AMX_BF16",
];

fn main() {
    println!("cargo:rerun-if-changed=CMakeLists.txt");
    println!("cargo:rerun-if-changed=shim.c");
    println!("cargo:rerun-if-changed=whisper.cpp");

    let mut config = cmake::Config::new(".");
    // A Debug ggml decodes many times slower; the worker is never useful so.
    config
        .profile("Release")
        .define("GGML_CPU", "ON")
        .define("GGML_NATIVE", "OFF");
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        for option in X86_64_ON {
            config.define(option, "ON");
        }
        for option in X86_64_OFF {
            config.define(option, "OFF");
        }
    }
    let out = config.build();

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
