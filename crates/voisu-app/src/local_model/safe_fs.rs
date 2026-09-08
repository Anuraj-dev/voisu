//! Directory-descriptor relative opens. Symlinks, FIFOs, and escapes fail.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SafeFsError {
    Escape,
    Symlink,
    NotRegular,
    Fifo,
    Device,
    UnsafeName,
    WorldWritable,
    NotFound,
    Io(String),
}

impl From<io::Error> for SafeFsError {
    fn from(error: io::Error) -> Self {
        if error.kind() == ErrorKind::NotFound || error.raw_os_error() == Some(libc::ENOENT) {
            Self::NotFound
        } else {
            Self::Io(error.to_string())
        }
    }
}

pub fn ensure_private_dir(path: &Path) -> Result<(), SafeFsError> {
    if let Some(parent) = path.parent() {
        check_ancestors(parent)?;
    }
    create_owned_components(path)
}

fn check_ancestors(mut path: &Path) -> Result<(), SafeFsError> {
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(SafeFsError::Symlink);
                }
                if !metadata.is_dir() {
                    return Err(SafeFsError::NotRegular);
                }
                let mode = metadata.mode();
                let other_write = mode & 0o002 != 0;
                let sticky = mode & 0o1000 != 0;
                if other_write && !sticky {
                    return Err(SafeFsError::WorldWritable);
                }
                if metadata.uid() == unsafe { libc::geteuid() } && mode & 0o022 != 0 && !sticky {
                    return Err(SafeFsError::WorldWritable);
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match path.parent() {
            Some(parent) if parent != path => path = parent,
            _ => break,
        }
    }
    Ok(())
}

fn create_owned_components(path: &Path) -> Result<(), SafeFsError> {
    let mut built = PathBuf::new();
    for component in path.components() {
        built.push(component.as_os_str());
        match component {
            Component::Prefix(_) | Component::RootDir => continue,
            Component::CurDir | Component::ParentDir => return Err(SafeFsError::Escape),
            Component::Normal(_) => {}
        }
        match fs::symlink_metadata(&built) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(SafeFsError::Symlink);
                }
                if !metadata.is_dir() {
                    return Err(SafeFsError::NotRegular);
                }
                let dir = open_dir(&built)?;
                drop(dir);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let parent = built.parent().ok_or(SafeFsError::Escape)?;
                let parent_dir = open_dir(parent)?;
                let name = built.file_name().ok_or(SafeFsError::UnsafeName)?;
                let c_name = CString::new(name.as_bytes()).map_err(|_| SafeFsError::UnsafeName)?;
                let created =
                    unsafe { libc::mkdirat(parent_dir.as_raw_fd(), c_name.as_ptr(), 0o700) };
                if created != 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() != ErrorKind::AlreadyExists {
                        return Err(err.into());
                    }
                }
                let dir = open_dir(&built)?;
                if dir.metadata()?.uid() != unsafe { libc::geteuid() } {
                    return Err(SafeFsError::Io(
                        "directory is not owned by the current user".into(),
                    ));
                }
                fs::set_permissions(&built, fs::Permissions::from_mode(0o700))?;
                if created == 0 {
                    // Crash before these fsyncs drops the new name and 0700 mode.
                    dir.sync_all()?;
                    parent_dir.sync_all()?;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub fn relative_file_name(name: &str) -> Result<&str, SafeFsError> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
    {
        return Err(SafeFsError::UnsafeName);
    }
    let path = Path::new(name);
    let mut parts = path.components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(os)), None) if os == name => Ok(name),
        _ => Err(SafeFsError::UnsafeName),
    }
}

pub fn reject_forbidden_filename(name: &str) -> Result<(), SafeFsError> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar")
        || lower.ends_with(".gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".zip")
        || lower.ends_with(".xz")
        || lower.ends_with(".zst")
        || lower.ends_with(".exe")
        || lower.ends_with(".so")
        || lower.ends_with(".sh")
        || lower.ends_with(".bin.sh")
    {
        return Err(SafeFsError::UnsafeName);
    }
    Ok(())
}

pub fn create_exclusive_file(dir: &Path, name: &str) -> Result<File, SafeFsError> {
    let name = relative_file_name(name)?;
    reject_forbidden_filename(name)?;
    let dir_file = open_dir(dir)?;
    let c_name = CString::new(name).map_err(|_| SafeFsError::UnsafeName)?;
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = open_relative(dir_file.as_raw_fd(), &c_name, flags, 0o600)?;
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn open_existing_file(dir: &Path, name: &str) -> Result<File, SafeFsError> {
    let name = relative_file_name(name)?;
    let dir_file = open_dir(dir)?;
    let c_name = CString::new(name).map_err(|_| SafeFsError::UnsafeName)?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NOCTTY;
    let fd = open_relative(dir_file.as_raw_fd(), &c_name, flags, 0)?;
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    reject_special(&metadata)?;
    if !metadata.is_file() {
        return Err(SafeFsError::NotRegular);
    }
    Ok(file)
}

pub fn read_existing_file(
    dir: &Path,
    name: &str,
    expected_bytes: u64,
) -> Result<Vec<u8>, SafeFsError> {
    let mut file = open_existing_file(dir, name)?;
    let mut bytes = Vec::new();
    let cap = expected_bytes
        .saturating_add(1)
        .try_into()
        .unwrap_or(usize::MAX);
    let mut buf = [0_u8; 8192];
    loop {
        if bytes.len() > cap {
            return Err(SafeFsError::Io("file exceeds catalog size".into()));
        }
        match file.read(&mut buf)? {
            0 => break,
            n => bytes.extend_from_slice(&buf[..n]),
        }
        if bytes.len() as u64 > expected_bytes {
            return Err(SafeFsError::Io("file exceeds catalog size".into()));
        }
    }
    Ok(bytes)
}

pub fn dir_present(path: &Path) -> Result<bool, SafeFsError> {
    match open_dir(path) {
        Ok(_) => Ok(true),
        Err(SafeFsError::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn inspect_tree(root: &Path) -> Result<(), SafeFsError> {
    let file = open_path_nofollow(root)?;
    inspect_opened(&file)
}

fn inspect_opened(file: &File) -> Result<(), SafeFsError> {
    let metadata = file.metadata()?;
    reject_special(&metadata)?;
    if metadata.is_file() {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(SafeFsError::NotRegular);
    }
    let dirfd = file.as_raw_fd();
    let proc = format!("/proc/self/fd/{dirfd}");
    for entry in fs::read_dir(&proc)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if name.as_bytes().contains(&b'/') || name.as_bytes().contains(&0) {
            return Err(SafeFsError::Escape);
        }
        let c_name = CString::new(name.as_bytes()).map_err(|_| SafeFsError::UnsafeName)?;
        let flags =
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK;
        let fd = open_relative(dirfd, &c_name, flags, 0)?;
        let child = unsafe { File::from_raw_fd(fd) };
        inspect_opened(&child)?;
    }
    Ok(())
}

pub fn list_regular_names(root: &Path) -> Result<Vec<String>, SafeFsError> {
    let dir = open_dir(root)?;
    let dirfd = dir.as_raw_fd();
    let proc = format!("/proc/self/fd/{dirfd}");
    let mut names = Vec::new();
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let utf = name.to_str().ok_or(SafeFsError::UnsafeName)?.to_owned();
        relative_file_name(&utf)?;
        names.push(utf);
    }
    names.sort();
    Ok(names)
}

fn reject_special(metadata: &fs::Metadata) -> Result<(), SafeFsError> {
    let ft = metadata.file_type();
    if ft.is_symlink() {
        return Err(SafeFsError::Symlink);
    }
    if ft.is_fifo() {
        return Err(SafeFsError::Fifo);
    }
    if ft.is_block_device() || ft.is_char_device() || ft.is_socket() {
        return Err(SafeFsError::Device);
    }
    Ok(())
}

pub fn durable_write(file: &mut File, bytes: &[u8]) -> Result<(), SafeFsError> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn durable_rename(from: &Path, to: &Path) -> Result<(), SafeFsError> {
    if dir_present(to)? {
        return Err(SafeFsError::Io(format!(
            "refusing to replace existing path {}",
            to.display()
        )));
    }
    match fs::symlink_metadata(to) {
        Ok(_) => {
            return Err(SafeFsError::Io(format!(
                "refusing to replace existing path {}",
                to.display()
            )));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // File contents are already synced; names in the staging directory are not.
    sync_dir(from)?;
    fs::rename(from, to)?;
    if let Some(parent) = to.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<(), SafeFsError> {
    open_dir(path)?.sync_all()?;
    Ok(())
}

fn open_path_nofollow(path: &Path) -> Result<File, SafeFsError> {
    open_walk(path, false)
}

fn open_dir(path: &Path) -> Result<File, SafeFsError> {
    open_walk(path, true)
}

fn open_walk(path: &Path, must_dir: bool) -> Result<File, SafeFsError> {
    let components: Vec<_> = path.components().collect();
    if components.is_empty() {
        return Err(SafeFsError::UnsafeName);
    }
    let mut current: Option<File> = None;
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        match component {
            Component::Prefix(_) | Component::ParentDir => return Err(SafeFsError::Escape),
            Component::CurDir => continue,
            Component::RootDir => {
                let fd = unsafe {
                    libc::open(
                        c"/".as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOCTTY,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                current = Some(unsafe { File::from_raw_fd(fd) });
            }
            Component::Normal(name) => {
                let c_name = CString::new(name.as_bytes()).map_err(|_| SafeFsError::UnsafeName)?;
                let mut flags =
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NOCTTY;
                if !last || must_dir {
                    flags |= libc::O_DIRECTORY;
                }
                if last && !must_dir {
                    flags |= libc::O_NONBLOCK;
                }
                let dirfd = current
                    .as_ref()
                    .map(File::as_raw_fd)
                    .unwrap_or(libc::AT_FDCWD);
                let fd = open_relative(dirfd, &c_name, flags, 0)?;
                current = Some(unsafe { File::from_raw_fd(fd) });
            }
        }
    }
    current.ok_or(SafeFsError::UnsafeName)
}

fn open_relative(
    dirfd: RawFd,
    name: &CString,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<RawFd, SafeFsError> {
    if let Some(fd) = openat2_relative(dirfd, name, flags, mode) {
        return fd;
    }
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(map_open_errno(io::Error::last_os_error()));
    }
    Ok(fd)
}

fn map_open_errno(error: io::Error) -> SafeFsError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => SafeFsError::Symlink,
        Some(libc::EXDEV) => SafeFsError::Escape,
        Some(libc::ENXIO) => SafeFsError::Fifo,
        Some(libc::ENOENT) => SafeFsError::NotFound,
        _ => error.into(),
    }
}

#[cfg(target_os = "linux")]
fn openat2_relative(
    dirfd: RawFd,
    name: &CString,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Option<Result<RawFd, SafeFsError>> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;
    let how = OpenHow {
        flags: flags as u64,
        mode: u64::from(mode),
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    };
    let sys_openat2: libc::c_long = 437;
    let fd = unsafe {
        libc::syscall(
            sys_openat2,
            dirfd,
            name.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOSYS) {
            return None;
        }
        return Some(Err(map_open_errno(error)));
    }
    Some(Ok(fd as RawFd))
}

#[cfg(not(target_os = "linux"))]
fn openat2_relative(
    dirfd: RawFd,
    name: &CString,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Option<Result<RawFd, SafeFsError>> {
    let _ = (dirfd, name, flags, mode);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn traversal_and_dot_names_are_rejected() {
        assert_eq!(relative_file_name("../etc"), Err(SafeFsError::UnsafeName));
        assert_eq!(relative_file_name("a/b"), Err(SafeFsError::UnsafeName));
        assert_eq!(relative_file_name(".hidden"), Err(SafeFsError::UnsafeName));
        assert_eq!(relative_file_name("model.bin"), Ok("model.bin"));
        assert!(reject_forbidden_filename("model.tar").is_err());
        assert!(reject_forbidden_filename("worker.so").is_err());
    }

    #[test]
    fn symlink_and_fifo_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        ensure_private_dir(root.path()).unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"x").unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(inspect_tree(&link), Err(SafeFsError::Symlink));

        let fifo = root.path().join("pipe");
        unsafe {
            let c = CString::new(fifo.to_str().unwrap()).unwrap();
            assert_eq!(libc::mkfifo(c.as_ptr(), 0o600), 0);
        }
        assert_eq!(inspect_tree(&fifo), Err(SafeFsError::Fifo));
    }

    #[test]
    fn world_writable_non_sticky_ancestor_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join("open");
        fs::create_dir(&ancestor).unwrap();
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o777)).unwrap();
        let child = ancestor.join("models");
        assert_eq!(ensure_private_dir(&child), Err(SafeFsError::WorldWritable));
    }

    #[test]
    fn inspect_opens_with_nofollow_directory_flags() {
        let source = include_str!("safe_fs.rs");
        assert!(source.contains("O_DIRECTORY"));
        assert!(source.contains("O_NOFOLLOW"));
        assert!(source.contains("RESOLVE_BENEATH"));
        assert!(source.contains("RESOLVE_NO_SYMLINKS"));
    }

    #[test]
    fn durable_rename_publishes_after_syncing_staging() {
        let root = tempfile::tempdir().unwrap();
        ensure_private_dir(root.path()).unwrap();
        let staging = root.path().join("staging").join("s1");
        ensure_private_dir(&staging).unwrap();
        {
            let mut file = create_exclusive_file(&staging, "model.bin").unwrap();
            durable_write(&mut file, b"abc").unwrap();
        }
        let dest = root
            .path()
            .join("artifacts")
            .join("id")
            .join("rev")
            .join("hash");
        ensure_private_dir(dest.parent().unwrap()).unwrap();
        durable_rename(&staging, &dest).unwrap();
        assert!(!staging.exists());
        let mut got = Vec::new();
        open_existing_file(&dest, "model.bin")
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"abc");

        let source = include_str!("safe_fs.rs");
        let rename_fn = source
            .split("pub fn durable_rename")
            .nth(1)
            .unwrap()
            .split("fn open_path_nofollow")
            .next()
            .unwrap();
        assert!(
            rename_fn.contains("sync_dir(from)"),
            "staging names must be durable before rename"
        );
        let mkdir = source
            .split("fn create_owned_components")
            .nth(1)
            .unwrap()
            .split("pub fn relative_file_name")
            .next()
            .unwrap();
        assert!(
            mkdir.contains("parent_dir.sync_all()"),
            "new catalog/revision names must be durable in their parents"
        );
    }
}
