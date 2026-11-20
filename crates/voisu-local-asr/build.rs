//! The worker links whisper.cpp's shared libraries, which ship beside it
//! (`target/<profile>/` in development, `/usr/libexec/voisu/` in packages,
//! the app's `local-asr\` folder on Windows, where the loader searches the
//! exe's directory and no rpath exists).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}
