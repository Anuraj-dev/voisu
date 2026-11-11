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

/// The `run: |` script of the step named `step` inside job `job`, dedented.
// The release step is a bash script, so this check runs on Unix only.
#[cfg(unix)]
fn workflow_step_script(workflow: &str, job: &str, step: &str) -> String {
    let job_start = workflow
        .find(&format!("\n  {job}:\n"))
        .unwrap_or_else(|| panic!("job {job} exists"));
    let job_text = &workflow[job_start..];
    let step_start = job_text
        .find(&format!("- name: {step}\n"))
        .unwrap_or_else(|| panic!("step {step} exists in {job}"));
    let mut lines = job_text[step_start..].lines();
    let run_line = lines
        .find(|line| line.trim_start() == "run: |")
        .expect("the step has a run block");
    let run_indent = run_line.len() - run_line.trim_start().len();
    lines
        .take_while(|line| {
            line.trim().is_empty() || line.len() - line.trim_start().len() > run_indent
        })
        .map(|line| line.get(run_indent + 2..).unwrap_or("").to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(unix)]
#[test]
fn every_release_attaches_the_linux_version_file() {
    let workflow = fs::read_to_string(workspace_root().join(".github/workflows/release.yml"))
        .expect("release workflow must be readable");
    // The job that publishes it must not wait on the Windows or macOS builds.
    let job = &workflow[workflow
        .find("\n  publish-release:\n")
        .expect("publish job")..];
    let needs = job
        .lines()
        .find(|line| line.trim_start().starts_with("needs:"))
        .expect("publish-release has needs");
    assert_eq!(needs.trim(), "needs: [validate, smoke]");
    assert!(
        job.contains("VERSION: ${{ needs.validate.outputs.version }}"),
        "the version comes from the validated tag"
    );

    // Run the real step with a fake `gh` that records what it was given.
    let script = workflow_step_script(
        &workflow,
        "publish-release",
        "Create the Release and attach assets",
    );
    let temp = tempfile::tempdir().expect("tempdir");
    let release = temp.path().join("dist/release");
    fs::create_dir_all(&release).unwrap();
    for asset in [
        "voisu_1.2.3_amd64.deb",
        "voisu-1.2.3-x86_64.tar.gz",
        "SHA256SUMS",
    ] {
        fs::write(release.join(asset), "").unwrap();
    }
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    let log = temp.path().join("gh.log");
    fs::write(
        &gh,
        format!("#!/bin/sh\necho \"$*\" > '{}'\n", log.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(temp.path())
        .env("PATH", path)
        .env("TAG", "v1.2.3")
        .env("VERSION", "1.2.3")
        .env("GITHUB_REPOSITORY", "owner/voisu")
        .status()
        .expect("bash runs");
    assert!(status.success(), "the step failed:\n{script}");

    assert_eq!(
        fs::read_to_string(release.join("latest-linux.json")).unwrap(),
        "{\"version\":\"1.2.3\"}\n"
    );
    let call = fs::read_to_string(&log).expect("gh ran");
    assert!(call.starts_with("release create v1.2.3 "), "{call}");
    assert!(call.trim_end().ends_with(" latest-linux.json"), "{call}");
}
