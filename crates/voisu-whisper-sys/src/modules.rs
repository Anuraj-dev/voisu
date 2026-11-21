//! A CPU-only backend registration, for workers that must not touch a GPU.
//!
//! `ggml_backend_load_all_from_path` opens and initializes every backend
//! module in the directory, Vulkan included, and ggml's `load_best` (which
//! could pick only the CPU) is static. So this mirrors `load_best("cpu")` at
//! the vendored tag: score every `<prefix>ggml-cpu-<variant><ext>` module,
//! register the best that runs here, else the plain `<prefix>ggml-cpu<ext>`.
//! Paths stay wide on Windows (`LoadLibraryW`), as ggml's own loader does.

use std::ffi::{CStr, OsStr, c_void};
use std::path::{Path, PathBuf};

#[cfg(windows)]
const PREFIX: &str = "ggml-cpu";
#[cfg(not(windows))]
const PREFIX: &str = "libggml-cpu";
#[cfg(windows)]
const EXTENSION: &str = ".dll";
#[cfg(not(windows))]
const EXTENSION: &str = ".so";

unsafe extern "C" {
    fn ggml_backend_register(reg: *mut c_void);
    fn ggml_backend_dev_count() -> usize;
}

/// `file_name` is a CPU variant module (`<prefix>-<variant><extension>`):
/// never the GPU modules beside it, nor the plain module.
pub(crate) fn is_cpu_variant(file_name: &str, prefix: &str, extension: &str) -> bool {
    file_name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(extension))
        .is_some_and(|variant| !variant.is_empty() && !variant.contains('.'))
}

/// Registers the best CPU variant in `dir`; the number of devices after.
pub(crate) fn load_cpu(dir: &Path) -> usize {
    let mut best: Option<(i32, PathBuf)> = None;
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    for path in entries.map(|entry| entry.path()) {
        let variant = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| is_cpu_variant(name, PREFIX, EXTENSION));
        if !variant || !path.is_file() {
            continue;
        }
        let Some(score) = Module::open(&path).and_then(|module| module.score()) else {
            continue;
        };
        if score > best.as_ref().map_or(0, |(top, _)| *top) {
            best = Some((score, path));
        }
    }
    let chosen = best.map_or_else(
        || dir.join(format!("{PREFIX}{EXTENSION}")),
        |(_, path)| path,
    );
    if let Some(module) = Module::open(&chosen) {
        module.register();
    }
    // SAFETY: plain registry read.
    unsafe { ggml_backend_dev_count() }
}

/// A loaded backend module, closed on drop unless registered.
struct Module(*mut c_void);

impl Module {
    fn symbol(&self, name: &CStr) -> *mut c_void {
        // SAFETY: the handle is open and `name` NUL-terminated.
        unsafe { sys::symbol(self.0, name) }
    }

    /// `ggml_backend_score`: 0 means the variant can't run on this CPU.
    fn score(&self) -> Option<i32> {
        let score = self.symbol(c"ggml_backend_score");
        if score.is_null() {
            return None;
        }
        // SAFETY: ggml exports it as `int (*)(void)`, and the module is open.
        unsafe {
            let score = std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> i32>(score);
            Some(score())
        }
    }

    /// `load_backend` at the tag: a module that scores 0 or has no init is
    /// skipped; otherwise its registry is added and the module stays loaded
    /// for the life of the process.
    fn register(self) {
        if self.score() == Some(0) {
            return;
        }
        let init = self.symbol(c"ggml_backend_init");
        if init.is_null() {
            return;
        }
        // SAFETY: ggml exports it as `ggml_backend_reg_t (*)(void)`. The
        // module stays loaded once registered (forgotten below), so the
        // registry it returns and its code remain valid.
        unsafe {
            let init =
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> *mut c_void>(init);
            let reg = init();
            if reg.is_null() {
                return;
            }
            ggml_backend_register(reg);
        }
        std::mem::forget(self);
    }
}

impl Module {
    fn open(path: &Path) -> Option<Self> {
        // SAFETY: loading a module beside the worker runs its initializers,
        // as ggml's own loader does.
        let handle = unsafe { sys::open(path) };
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: the handle is open and closed once.
        unsafe { sys::close(self.0) };
    }
}

#[cfg(unix)]
mod sys {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const RTLD_NOW: c_int = 2;

    unsafe extern "C" {
        fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
    }

    pub unsafe fn open(path: &Path) -> *mut c_void {
        match CString::new(path.as_os_str().as_bytes()) {
            Ok(path) => unsafe { dlopen(path.as_ptr(), RTLD_NOW) },
            Err(_) => std::ptr::null_mut(),
        }
    }

    pub unsafe fn symbol(handle: *mut c_void, name: &CStr) -> *mut c_void {
        unsafe { dlsym(handle, name.as_ptr()) }
    }

    pub unsafe fn close(handle: *mut c_void) {
        unsafe { dlclose(handle) };
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::{CStr, c_char, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    /// No "missing DLL" dialog: nobody would see it, and it would hang the
    /// worker. ggml's loader sets the same.
    const SEM_FAILCRITICALERRORS: u32 = 0x0001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn SetErrorMode(mode: u32) -> u32;
    }

    pub unsafe fn open(path: &Path) -> *mut c_void {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            let old = SetErrorMode(SEM_FAILCRITICALERRORS);
            SetErrorMode(old | SEM_FAILCRITICALERRORS);
            let handle = LoadLibraryW(wide.as_ptr());
            SetErrorMode(old);
            handle
        }
    }

    pub unsafe fn symbol(handle: *mut c_void, name: &CStr) -> *mut c_void {
        unsafe { GetProcAddress(handle, name.as_ptr()) }
    }

    pub unsafe fn close(handle: *mut c_void) {
        unsafe { FreeLibrary(handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::is_cpu_variant;

    #[test]
    fn only_cpu_variant_modules_are_candidates() {
        for (name, prefix, extension) in [
            ("ggml-cpu-haswell.dll", "ggml-cpu", ".dll"),
            ("ggml-cpu-x64.dll", "ggml-cpu", ".dll"),
            ("libggml-cpu-sapphirerapids.so", "libggml-cpu", ".so"),
        ] {
            assert!(is_cpu_variant(name, prefix, extension), "{name}");
        }
        for (name, prefix, extension) in [
            ("ggml-vulkan.dll", "ggml-cpu", ".dll"),
            ("libggml-vulkan.so", "libggml-cpu", ".so"),
            ("ggml-cpu.dll", "ggml-cpu", ".dll"),
            ("ggml-cpu-.dll", "ggml-cpu", ".dll"),
            ("ggml-cpu-haswell.dll.bak", "ggml-cpu", ".dll"),
            ("ggml-cpu-haswell.pdb", "ggml-cpu", ".dll"),
            ("ggml-cuda-x64.dll", "ggml-cpu", ".dll"),
        ] {
            assert!(!is_cpu_variant(name, prefix, extension), "{name}");
        }
    }
}
