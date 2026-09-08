//! Directory-descriptor relative opens. Symlinks, FIFOs, and escapes fail.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SafeFsError {
    Escape,
    Symlink,
    NotRegular,
    Fifo,
    Device,
    UnsafeName,
    Io(String),
}

impl From<io::Error> for SafeFsError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

pub fn ensure_private_dir(path: &Path) -> Result<(), SafeFsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(SafeFsError::Symlink);
            }
            if !metadata.is_dir() {
                return Err(SafeFsError::NotRegular);
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(SafeFsError::Io(
                    "directory is not owned by the current user".into(),
                ));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(path)?;
        }
        Err(error) => return Err(error.into()),
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

pub fn inspect_tree(root: &Path) -> Result<(), SafeFsError> {
    inspect_tree_at(root, Path::new(""))
}

fn inspect_tree_at(root: &Path, rel: &Path) -> Result<(), SafeFsError> {
    let path = if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() {
        return Err(SafeFsError::Symlink);
    }
    if metadata.file_type().is_fifo() {
        return Err(SafeFsError::Fifo);
    }
    if metadata.file_type().is_block_device() || metadata.file_type().is_char_device() {
        return Err(SafeFsError::Device);
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            if name.as_bytes().contains(&b'/') {
                return Err(SafeFsError::Escape);
            }
            inspect_tree_at(root, &rel.join(name))?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(SafeFsError::NotRegular);
    }
    Ok(())
}

pub fn durable_write(file: &mut File, bytes: &[u8]) -> Result<(), SafeFsError> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn durable_rename(from: &Path, to: &Path) -> Result<(), SafeFsError> {
    if to.exists() {
        return Err(SafeFsError::Io(format!(
            "refusing to replace existing path {}",
            to.display()
        )));
    }
    fs::rename(from, to)?;
    if let Some(parent) = to.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn open_dir(path: &Path) -> Result<File, SafeFsError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                SafeFsError::Symlink
            } else {
                error.into()
            }
        })
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
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ELOOP) {
            return Err(SafeFsError::Symlink);
        }
        return Err(error.into());
    }
    Ok(fd)
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
        if error.raw_os_error() == Some(libc::ELOOP) || error.raw_os_error() == Some(libc::EXDEV) {
            return Some(Err(SafeFsError::Escape));
        }
        return Some(Err(error.into()));
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
    use std::os::unix::net::UnixDatagram;

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
        let _ = UnixDatagram::unbound();
    }
}
