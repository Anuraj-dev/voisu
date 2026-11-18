//! The built worker is shaped the way every package ships it: RUNPATH
//! `$ORIGIN` and each file `voisu-whisper-runtime.txt` lists beside it. The
//! same script checks the packaged directories.

use std::path::Path;
use std::process::Command;

#[test]
fn the_built_worker_finds_only_its_listed_private_runtime() {
    let worker = Path::new(env!("CARGO_BIN_EXE_voisu-local-asr"));
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/ci/check-worker-runtime.sh");
    let output = Command::new("bash")
        .arg(&script)
        .arg(worker.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
