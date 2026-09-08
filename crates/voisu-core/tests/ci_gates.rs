//! L0 verification floor for CI lockfiles and the undeclared toolchain matrix.
//!
//! These tests encode the intended commands and named defects. They read the
//! committed workflow; they do not spawn cargo.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/voisu-core lives two levels below the workspace")
        .to_path_buf()
}

fn ci_workflow() -> String {
    fs::read_to_string(workspace_root().join(".github/workflows/ci.yml"))
        .expect("CI workflow must be readable")
}

#[test]
fn workspace_test_gate_uses_committed_lockfile() {
    // Intended default test gate: cargo test --workspace --locked
    let ci = ci_workflow();
    assert!(
        ci.contains("cargo test --workspace --locked"),
        "default CI test gate must consume Cargo.lock: cargo test --workspace --locked"
    );
    assert!(
        ci.contains("cargo test --manifest-path tools/transcript-quality/Cargo.toml --locked"),
        "transcript-quality CI must stay --locked"
    );
}

#[test]
fn toolchain_matrix_is_named_as_an_l6_blocker() {
    // L0-DEFECT / L6 blocker: no rust-toolchain.toml. Default CI jobs float on
    // dtolnay/rust-toolchain@stable; the MSRV job pins 1.92.0 to match
    // workspace rust-version = "1.92". L6 must pin the default jobs to a
    // concrete channel or fail closed. Do not bump MSRV here.
    let root = workspace_root();
    assert!(
        !root.join("rust-toolchain.toml").is_file(),
        "rust-toolchain.toml is absent on purpose until default CI jobs stop floating on @stable"
    );
    let cargo = fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml");
    assert!(
        cargo.contains("rust-version = \"1.92\""),
        "workspace MSRV must stay 1.92"
    );
    let ci = ci_workflow();
    assert!(
        ci.contains("dtolnay/rust-toolchain@stable"),
        "default CI jobs currently float on @stable"
    );
    assert!(
        ci.contains("dtolnay/rust-toolchain@1.92.0"),
        "MSRV job must stay pinned at 1.92.0"
    );
    assert!(
        ci.contains("L0-DEFECT"),
        "CI must name the undeclared toolchain matrix as an L6 blocker"
    );
}
