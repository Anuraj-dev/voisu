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

fn uses_line_contains(ci: &str, needle: &str) -> bool {
    ci.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("- uses:") && trimmed.contains(needle)
    })
}

#[test]
fn workspace_test_gate_uses_committed_lockfile() {
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
fn msrv_job_is_pinned_to_1_92_0() {
    let cargo =
        fs::read_to_string(workspace_root().join("Cargo.toml")).expect("workspace Cargo.toml");
    assert!(
        cargo.contains("rust-version = \"1.92\""),
        "workspace MSRV must stay 1.92"
    );
    assert!(
        uses_line_contains(&ci_workflow(), "dtolnay/rust-toolchain@1.92.0"),
        "MSRV job must pin dtolnay/rust-toolchain@1.92.0 on a uses: line"
    );
}

#[test]
fn default_ci_jobs_pin_stable_on_the_uses_line() {
    assert!(
        uses_line_contains(&ci_workflow(), "dtolnay/rust-toolchain@stable"),
        "default CI jobs must use dtolnay/rust-toolchain@stable on a uses: line"
    );
}

#[ignore = "L0-DEFECT: rust-toolchain.toml does not pin default CI jobs"]
#[test]
fn rust_toolchain_toml_pins_the_default_ci_channel() {
    assert!(
        workspace_root().join("rust-toolchain.toml").is_file(),
        "default CI jobs should be pinned by rust-toolchain.toml"
    );
}
