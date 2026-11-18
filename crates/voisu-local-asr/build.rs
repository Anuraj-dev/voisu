//! The worker links whisper.cpp's shared libraries, which ship beside it
//! (`target/<profile>/` in development, `/usr/libexec/voisu/` in packages).
fn main() {
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}
