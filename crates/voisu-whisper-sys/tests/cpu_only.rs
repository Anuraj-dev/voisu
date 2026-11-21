//! The CPU-only registration never opens a GPU module. Its own test binary:
//! backends register once per process.

use std::path::PathBuf;

use voisu_whisper_sys as sys;

#[test]
fn a_cpu_load_never_opens_the_vulkan_module() {
    let (prefix, extension) = if cfg!(windows) {
        ("ggml-", ".dll")
    } else {
        ("libggml-", ".so")
    };
    let runtime = PathBuf::from(env!("VOISU_WHISPER_RUNTIME_DIR"));
    let dir = std::env::temp_dir().join(format!("voisu-cpu-only-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut planted = false;
    for entry in std::fs::read_dir(&runtime).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{prefix}cpu")) && name.ends_with(extension) {
            std::fs::copy(entry.path(), dir.join(&name)).unwrap();
            // A loadable module where Vulkan's would be: had it been opened,
            // it would register a second device.
            if !planted {
                std::fs::copy(entry.path(), dir.join(format!("{prefix}vulkan{extension}")))
                    .unwrap();
                planted = true;
            }
        }
    }
    assert!(planted, "no CPU backend module in {}", runtime.display());
    let devices = sys::load_cpu_backend(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(devices, 1);
    assert!(unsafe { sys::voisu_whisper_cpu_backend_loaded() });
    assert_eq!(unsafe { sys::voisu_whisper_non_cpu_devices() }, 0);
}
