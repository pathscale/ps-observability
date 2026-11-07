use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

pub(crate) fn default_directory() -> PathBuf {
    let name = if cfg!(target_os = "macos") {
        "tauri-blitz-agent".to_owned()
    } else {
        format!("tauri-blitz-agent-{}", rustix::process::geteuid().as_raw())
    };
    std::env::temp_dir().join(name)
}

fn check_private_metadata(meta: &Metadata) -> io::Result<()> {
    if meta.file_type().is_symlink()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control paths must be nonsymlink paths owned by the effective uid with no group or other permissions",
        ));
    }
    Ok(())
}

pub(crate) fn validate_directory(path: &Path) -> io::Result<PathBuf> {
    let before = std::fs::symlink_metadata(path)?;
    check_private_metadata(&before)?;
    if !before.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control directory must be a directory",
        ));
    }

    // Resolve parent aliases such as macOS /var and /tmp only after lstat.
    let canonical = path.canonicalize()?;
    let after = std::fs::symlink_metadata(&canonical)?;
    check_private_metadata(&after)?;
    if !after.is_dir() || before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control directory changed during validation",
        ));
    }
    Ok(canonical)
}

pub(crate) fn open_descriptor(path: &Path) -> io::Result<File> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = validate_directory(parent)?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "descriptor needs a file name")
    })?;
    let path = directory.join(name);
    let before = std::fs::symlink_metadata(&path)?;
    check_private_metadata(&before)?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor must be a regular file",
        ));
    }

    let fd = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    let file = File::from(fd);
    let after = file.metadata()?;
    check_private_metadata(&after)?;
    if !after.is_file() || before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "descriptor changed during validation",
        ));
    }
    Ok(file)
}

pub(crate) fn validate_socket(directory: &Path, address: &str) -> io::Result<PathBuf> {
    let directory = validate_directory(directory)?;
    let socket = Path::new(address.strip_prefix("unix://").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor address must use unix://",
        )
    })?);
    if !socket.is_absolute()
        || socket
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor socket must be an absolute path without parent traversal",
        ));
    }
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket needs a parent"))?;
    if validate_directory(parent)? != directory {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "descriptor socket must reside inside its control directory",
        ));
    }
    let name = socket
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket needs a file name"))?;
    let socket = directory.join(name);
    let meta = std::fs::symlink_metadata(&socket)?;
    check_private_metadata(&meta)?;
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor address must name a Unix socket",
        ));
    }
    Ok(socket)
}
