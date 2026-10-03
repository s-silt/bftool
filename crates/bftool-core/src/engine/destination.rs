//! No-follow destination write boundary. Callers must still choose an authorized root.
//!
//! Linux mutations are relative to open directory descriptors, never resolved through
//! a checked-then-followed pathname. Windows keeps non-reparse ancestors open without
//! write/delete sharing while using child paths. Unsupported platforms fail closed.
//! These guards defend against symlink/reparse substitution; they do not give an
//! untrusted process permission to move a mounted root or bypass OS access controls.

use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) struct SafeDir {
    path: PathBuf,
    handle: platform::Directory,
}

fn normal_components(path: &Path) -> Result<Vec<OsString>> {
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                // Reject Windows alternate streams/separators even on Linux, where
                // they would otherwise become portable manifests with unsafe names.
                let s = name.to_string_lossy();
                if s.contains('\\') || s.contains(':') || s.contains('\0') {
                    bail!("不安全的目标路径分量：{}", path.display());
                }
                #[cfg(windows)]
                {
                    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
                    let numbered_device = (stem.starts_with("COM") || stem.starts_with("LPT"))
                        && stem.len() == 4
                        && matches!(stem.as_bytes()[3], b'1'..=b'9');
                    if (s.ends_with('.') || s.ends_with(' '))
                        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                        || numbered_device
                    {
                        bail!("目标路径含Windows设备名或别名分量：{}", path.display());
                    }
                }
                names.push(name.to_owned());
            }
            Component::CurDir => {}
            _ => bail!("目标路径必须是根内相对路径：{}", path.display()),
        }
    }
    Ok(names)
}

impl SafeDir {
    /// Open every ancestor without following links. `create` creates missing directories.
    pub(crate) fn open(path: &Path, create: bool) -> Result<Self> {
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            bail!("目标路径含上级跳转，拒绝写入：{}", path.display());
        }
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut root = PathBuf::new();
        let mut rest = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(prefix) => root.push(prefix.as_os_str()),
                Component::RootDir => root.push(component.as_os_str()),
                Component::Normal(name) => rest.push(name),
                Component::CurDir => {}
                Component::ParentDir => bail!("目标路径含上级跳转"),
            }
        }
        let mut dir = Self {
            handle: platform::open_root(&root)
                .with_context(|| format!("安全打开目标根失败：{}", root.display()))?,
            path: root,
        };
        // Ancestors may have native names not suitable as portable payload names
        // (for example the Linux synthetic "T:\" fixture). They are still opened
        // component by component, with parent traversal already rejected above.
        for component in rest.components() {
            let Component::Normal(name) = component else {
                bail!("目标祖先路径无效");
            };
            let child_path = dir.path.join(name);
            if create {
                platform::mkdir(&dir.handle, &child_path, name, false)?;
            }
            let handle = platform::open_child(&dir.handle, &child_path, name)
                .with_context(|| format!("目标祖先不是普通目录：{}", child_path.display()))?;
            dir = Self {
                path: child_path,
                handle,
            };
        }
        Ok(dir)
    }

    fn descend(&self, relative: &Path, create: bool, exclusive_last: bool) -> Result<Self> {
        let names = normal_components(relative)?;
        if exclusive_last && names.is_empty() {
            bail!("新目标目录名不能为空");
        }
        let mut dir = Self {
            path: self.path.clone(),
            handle: platform::clone_dir(&self.handle)?,
        };
        for (index, name) in names.iter().enumerate() {
            let child_path = dir.path.join(name);
            let exclusive = exclusive_last && index + 1 == names.len();
            if create {
                platform::mkdir(&dir.handle, &child_path, name, exclusive)
                    .with_context(|| format!("安全创建目标目录失败：{}", child_path.display()))?;
            }
            let handle =
                platform::open_child(&dir.handle, &child_path, name).with_context(|| {
                    format!("目标目录不是可安全打开的普通目录：{}", child_path.display())
                })?;
            dir = Self {
                path: child_path,
                handle,
            };
        }
        Ok(dir)
    }

    /// A stable pathname for reads of this exact open directory object. On Linux
    /// the parent process owns the descriptor so archive-test children can use it.
    pub(crate) fn anchored_path(&self) -> PathBuf {
        platform::anchored_path(&self.handle, &self.path)
    }

    /// Refuse a replaced or relinked visible directory before consequential stages.
    pub(crate) fn require_current_binding(&self) -> Result<()> {
        let current = Self::open(&self.path, false)?;
        if !platform::same_dir(&self.handle, &current.handle)? {
            bail!(
                "目标目录对象已被替换，保留源与现有目标并停止：{}",
                self.path.display()
            );
        }
        Ok(())
    }

    pub(crate) fn ensure_dir(&self, relative: &Path) -> Result<Self> {
        self.require_current_binding()?;
        self.descend(relative, true, false)
    }

    pub(crate) fn create_new_dir(&self, relative: &Path) -> Result<Self> {
        self.require_current_binding()?;
        self.descend(relative, true, true)
    }

    fn parent_and_name(&self, relative: &Path, create: bool) -> Result<(Self, OsString)> {
        let mut names = normal_components(relative)?;
        let name = names.pop().context("目标文件名不能为空")?;
        let parent: PathBuf = names.into_iter().collect();
        Ok((self.descend(&parent, create, false)?, name))
    }

    fn create_temp(&self, target: &OsStr, suffix: &str) -> Result<(OsString, File)> {
        for _ in 0..32 {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let mut name = target.to_os_string();
            name.push(format!(
                ".{}.{}.{}{}",
                std::process::id(),
                stamp,
                sequence,
                suffix
            ));
            match platform::create_file(&self.handle, &self.path.join(&name), &name) {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("安全创建唯一临时文件失败"),
            }
        }
        bail!("无法分配唯一临时文件，拒绝复用或删除未知文件")
    }

    pub(crate) fn read_regular(&self, relative: &Path) -> Result<File> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        platform::read_file(&parent.handle, &parent.path.join(&name), &name)
            .with_context(|| format!("安全打开目标普通文件失败：{}", relative.display()))
    }

    pub(crate) fn append_synced(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        let mut file = platform::append_file(&parent.handle, &parent.path.join(&name), &name)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        platform::sync_dir(&parent.handle)?;
        Ok(())
    }

    pub(crate) fn write_atomic(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        self.write_atomic_mode(relative, bytes, false)
    }

    pub(crate) fn write_atomic_new(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        self.write_atomic_mode(relative, bytes, true)
    }

    fn write_atomic_mode(&self, relative: &Path, bytes: &[u8], no_replace: bool) -> Result<()> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        let (tmp, mut file) = parent.create_temp(&name, ".bftool-tmp")?;
        let result = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            if no_replace {
                platform::rename_new(
                    &parent.handle,
                    &parent.path,
                    &tmp,
                    &parent.handle,
                    &parent.path,
                    &name,
                )?;
            } else {
                platform::replace_file(&parent.handle, &parent.path, &tmp, &name)?;
            }
            platform::sync_dir(&parent.handle)?;
            self.require_current_binding()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = platform::unlink_file(&parent.handle, &parent.path.join(&tmp), &tmp);
        }
        result.with_context(|| format!("安全原子写入失败：{}", parent.path.join(name).display()))
    }

    /// Publish a freshly copied payload without ever replacing an occupied name.
    pub(crate) fn copy_new(
        &self,
        src: &Path,
        relative: &Path,
        no_hash: bool,
    ) -> Result<Option<String>> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, true)?;
        let (tmp, mut destination) =
            parent.create_temp(&name, crate::engine::cruft::PART_SUFFIX)?;
        let result = (|| -> Result<Option<String>> {
            let mut source =
                File::open(src).with_context(|| format!("打开复制源失败：{}", src.display()))?;
            let mut hasher = Sha256::new();
            let mut buffer = vec![0; 1024 * 1024];
            loop {
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                destination.write_all(&buffer[..count])?;
                if !no_hash {
                    hasher.update(&buffer[..count]);
                }
            }
            destination.sync_all()?;
            drop(destination);
            platform::rename_new(
                &parent.handle,
                &parent.path,
                &tmp,
                &parent.handle,
                &parent.path,
                &name,
            )?;
            platform::sync_dir(&parent.handle)?;
            self.require_current_binding()?;
            Ok((!no_hash).then(|| format!("{:X}", hasher.finalize())))
        })();
        if result.is_err() {
            let _ = platform::unlink_file(&parent.handle, &parent.path.join(&tmp), &tmp);
        }
        result.with_context(|| {
            format!(
                "安全复制失败或目标已被占用：{}",
                parent.path.join(name).display()
            )
        })
    }

    pub(crate) fn remove_regular(&self, relative: &Path) -> Result<()> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        platform::require_regular(&parent.handle, &parent.path.join(&name), &name)?;
        platform::unlink_file(&parent.handle, &parent.path.join(&name), &name)?;
        platform::sync_dir(&parent.handle)?;
        Ok(())
    }

    /// Move an ordinary source file or directory to an unoccupied archive name.
    /// Both parents stay anchored; source links/reparse points are refused.
    pub(crate) fn rename_entry_to(&self, from: &Path, to_dir: &Self, to: &Path) -> Result<()> {
        self.require_current_binding()?;
        to_dir.require_current_binding()?;
        let (source_parent, source_name) = self.parent_and_name(from, false)?;
        let (destination_parent, destination_name) = to_dir.parent_and_name(to, true)?;
        platform::require_entry(
            &source_parent.handle,
            &source_parent.path.join(&source_name),
            &source_name,
        )?;
        platform::rename_new(
            &source_parent.handle,
            &source_parent.path,
            &source_name,
            &destination_parent.handle,
            &destination_parent.path,
            &destination_name,
        )?;
        platform::sync_dir(&source_parent.handle)?;
        platform::sync_dir(&destination_parent.handle)?;
        Ok(())
    }
}

#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
))]
mod platform {
    use super::*;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    pub(super) struct Directory(File);
    const O_RDONLY: i32 = 0;
    const O_WRONLY: i32 = 1;
    const O_CREAT: i32 = 0o100;
    const O_EXCL: i32 = 0o200;
    // Linux fcntl flag values differ on ARM; do not silently apply x86 flags.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    const O_DIRECTORY: i32 = 0x10000;
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    const O_NOFOLLOW: i32 = 0x20000;
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    const O_DIRECTORY: i32 = 0x4000;
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    const O_NOFOLLOW: i32 = 0x8000;
    const O_APPEND: i32 = 0o2000;
    const O_CLOEXEC: i32 = 0o2000000;
    const O_NONBLOCK: i32 = 0o4000;
    const RENAME_NOREPLACE: u32 = 1;
    unsafe extern "C" {
        fn openat(fd: i32, path: *const std::ffi::c_char, flags: i32, mode: u32) -> i32;
        fn mkdirat(fd: i32, path: *const std::ffi::c_char, mode: u32) -> i32;
        fn unlinkat(fd: i32, path: *const std::ffi::c_char, flags: i32) -> i32;
        fn renameat(
            fd: i32,
            from: *const std::ffi::c_char,
            to_fd: i32,
            to: *const std::ffi::c_char,
        ) -> i32;
        fn renameat2(
            fd: i32,
            from: *const std::ffi::c_char,
            to_fd: i32,
            to: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filename"))
    }
    fn open_relative(parent: &Directory, name: &OsStr, flags: i32) -> io::Result<File> {
        let name = c_name(name)?;
        // SAFETY: name is a live NUL-terminated string, parent owns a live descriptor;
        // any nonnegative returned descriptor is transferred exactly once to File.
        let fd = unsafe {
            openat(
                parent.0.as_raw_fd(),
                name.as_ptr(),
                flags | O_NOFOLLOW | O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub(super) fn open_root(path: &Path) -> io::Result<Directory> {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(Directory(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)
                .open(path)?,
        ))
    }
    pub(super) fn anchored_path(parent: &Directory, _path: &Path) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            parent.0.as_raw_fd()
        ))
    }
    pub(super) fn same_dir(a: &Directory, b: &Directory) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let a = a.0.metadata()?;
        let b = b.0.metadata()?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    pub(super) fn clone_dir(parent: &Directory) -> io::Result<Directory> {
        Ok(Directory(parent.0.try_clone()?))
    }
    pub(super) fn open_child(
        parent: &Directory,
        _path: &Path,
        name: &OsStr,
    ) -> io::Result<Directory> {
        Ok(Directory(open_relative(
            parent,
            name,
            O_RDONLY | O_DIRECTORY,
        )?))
    }
    pub(super) fn mkdir(
        parent: &Directory,
        _path: &Path,
        name: &OsStr,
        exclusive: bool,
    ) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: descriptor and C string are live; mkdirat follows no leaf link.
        if unsafe { mkdirat(parent.0.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !exclusive && error.kind() == io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(error)
        }
    }
    pub(super) fn create_file(parent: &Directory, _path: &Path, name: &OsStr) -> io::Result<File> {
        open_relative(parent, name, O_WRONLY | O_CREAT | O_EXCL)
    }
    pub(super) fn read_file(parent: &Directory, _path: &Path, name: &OsStr) -> io::Result<File> {
        let file = open_relative(parent, name, O_RDONLY | O_NONBLOCK)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read destination is not a regular file",
            ));
        }
        Ok(file)
    }
    pub(super) fn append_file(parent: &Directory, _path: &Path, name: &OsStr) -> io::Result<File> {
        let file = open_relative(parent, name, O_WRONLY | O_APPEND | O_CREAT | O_NONBLOCK)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "append destination is not a regular file",
            ));
        }
        Ok(file)
    }
    pub(super) fn require_entry(parent: &Directory, _path: &Path, name: &OsStr) -> io::Result<()> {
        let file = open_relative(parent, name, O_RDONLY | O_NONBLOCK)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source is not an ordinary file or directory",
            ));
        }
        Ok(())
    }
    pub(super) fn require_regular(
        parent: &Directory,
        _path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        let file = open_relative(parent, name, O_RDONLY | O_NONBLOCK)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "target is not a regular file",
            ));
        }
        Ok(())
    }
    pub(super) fn unlink_file(parent: &Directory, _path: &Path, name: &OsStr) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: descriptor and C string remain live throughout the call.
        if unsafe { unlinkat(parent.0.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn rename_new(
        from: &Directory,
        _from_path: &Path,
        from_name: &OsStr,
        to: &Directory,
        _to_path: &Path,
        to_name: &OsStr,
    ) -> io::Result<()> {
        let from_name = c_name(from_name)?;
        let to_name = c_name(to_name)?;
        // SAFETY: both descriptors and both C strings are live; RENAME_NOREPLACE
        // atomically refuses any existing destination, including a dangling link.
        if unsafe {
            renameat2(
                from.0.as_raw_fd(),
                from_name.as_ptr(),
                to.0.as_raw_fd(),
                to_name.as_ptr(),
                RENAME_NOREPLACE,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn replace_file(
        parent: &Directory,
        path: &Path,
        from: &OsStr,
        to: &OsStr,
    ) -> io::Result<()> {
        match require_regular(parent, &path.join(to), to) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let from = c_name(from)?;
        let to = c_name(to)?;
        // SAFETY: descriptors and C strings are live. rename replaces a leaf name,
        // never follows a substituted leaf symlink to its referent.
        if unsafe {
            renameat(
                parent.0.as_raw_fd(),
                from.as_ptr(),
                parent.0.as_raw_fd(),
                to.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn sync_dir(parent: &Directory) -> io::Result<()> {
        parent.0.sync_all()
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::sync::Arc;

    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    pub(super) struct Directory {
        held: Vec<Arc<File>>,
    }
    fn ordinary(file: &File, directory: bool) -> io::Result<()> {
        let metadata = file.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || metadata.is_dir() != directory
            || (!directory && !metadata.is_file())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "target is a reparse point or wrong file type",
            ));
        }
        Ok(())
    }
    fn open_directory(path: &Path) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        ordinary(&file, true)?;
        Ok(file)
    }
    pub(super) fn open_root(path: &Path) -> io::Result<Directory> {
        Ok(Directory {
            held: vec![Arc::new(open_directory(path)?)],
        })
    }
    pub(super) fn anchored_path(_parent: &Directory, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
    pub(super) fn same_dir(_a: &Directory, _b: &Directory) -> io::Result<bool> {
        // All ancestors and this directory remain open without write/delete
        // sharing, so their visible binding cannot be replaced while held.
        Ok(true)
    }
    pub(super) fn clone_dir(parent: &Directory) -> io::Result<Directory> {
        Ok(Directory {
            held: parent.held.clone(),
        })
    }
    pub(super) fn open_child(
        parent: &Directory,
        path: &Path,
        _name: &OsStr,
    ) -> io::Result<Directory> {
        let file = open_directory(path)?;
        let mut held = parent.held.clone();
        held.push(Arc::new(file));
        Ok(Directory { held })
    }
    pub(super) fn mkdir(
        _parent: &Directory,
        path: &Path,
        _name: &OsStr,
        exclusive: bool,
    ) -> io::Result<()> {
        match std::fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if !exclusive && error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }
    pub(super) fn create_file(_parent: &Directory, path: &Path, _name: &OsStr) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        ordinary(&file, false)?;
        Ok(file)
    }
    pub(super) fn read_file(_parent: &Directory, path: &Path, _name: &OsStr) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        ordinary(&file, false)?;
        Ok(file)
    }
    pub(super) fn append_file(_parent: &Directory, path: &Path, _name: &OsStr) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .share_mode(FILE_SHARE_READ | 2)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        ordinary(&file, false)?;
        Ok(file)
    }
    pub(super) fn require_entry(_parent: &Directory, path: &Path, _name: &OsStr) -> io::Result<()> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | 2 | 4)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let metadata = file.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (!metadata.is_file() && !metadata.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source is a reparse point or special entry",
            ));
        }
        Ok(())
    }
    pub(super) fn require_regular(
        _parent: &Directory,
        path: &Path,
        _name: &OsStr,
    ) -> io::Result<()> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | 2 | 4)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        ordinary(&file, false)
    }
    pub(super) fn unlink_file(_parent: &Directory, path: &Path, _name: &OsStr) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(from: *const u16, to: *const u16) -> i32;
    }
    pub(super) fn rename_new(
        _from: &Directory,
        from_path: &Path,
        from: &OsStr,
        _to: &Directory,
        to_path: &Path,
        to: &OsStr,
    ) -> io::Result<()> {
        let from: Vec<u16> = from_path
            .join(from)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let to: Vec<u16> = to_path
            .join(to)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: terminated UTF-16 buffers remain live. MoveFileW cannot replace
        // existing destinations; ancestors are held without write/delete sharing.
        if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn replace_file(
        parent: &Directory,
        path: &Path,
        from: &OsStr,
        to: &OsStr,
    ) -> io::Result<()> {
        match require_regular(parent, &path.join(to), to) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::rename(path.join(from), path.join(to))
    }
    // File-level FlushFileBuffers is used; Windows directory fsync is unsupported.
    pub(super) fn sync_dir(_parent: &Directory) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(any(
    windows,
    all(
        target_os = "linux",
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64"
        )
    )
)))]
mod platform {
    use super::*;
    use std::io;
    pub(super) struct Directory;
    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe destination writes are only implemented for Windows and supported Linux x86/ARM architectures",
        ))
    }
    pub(super) fn open_root(_: &Path) -> io::Result<Directory> {
        unsupported()
    }
    pub(super) fn anchored_path(_: &Directory, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
    pub(super) fn same_dir(_: &Directory, _: &Directory) -> io::Result<bool> {
        unsupported()
    }
    pub(super) fn clone_dir(_: &Directory) -> io::Result<Directory> {
        unsupported()
    }
    pub(super) fn open_child(_: &Directory, _: &Path, _: &OsStr) -> io::Result<Directory> {
        unsupported()
    }
    pub(super) fn mkdir(_: &Directory, _: &Path, _: &OsStr, _: bool) -> io::Result<()> {
        unsupported()
    }
    pub(super) fn create_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub(super) fn read_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub(super) fn append_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub(super) fn require_entry(_: &Directory, _: &Path, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub(super) fn require_regular(_: &Directory, _: &Path, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub(super) fn unlink_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub(super) fn rename_new(
        _: &Directory,
        _: &Path,
        _: &OsStr,
        _: &Directory,
        _: &Path,
        _: &OsStr,
    ) -> io::Result<()> {
        unsupported()
    }
    pub(super) fn replace_file(_: &Directory, _: &Path, _: &OsStr, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub(super) fn sync_dir(_: &Directory) -> io::Result<()> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_and_existing_targets_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let safe = SafeDir::open(temp.path(), false).unwrap();
        assert!(safe.ensure_dir(Path::new("../outside")).is_err());
        let source = temp.path().join("source.txt");
        std::fs::write(&source, b"NEW").unwrap();
        std::fs::write(temp.path().join("existing.txt"), b"OLD-UNRELATED").unwrap();
        assert!(safe
            .copy_new(&source, Path::new("existing.txt"), false)
            .is_err());
        assert_eq!(
            std::fs::read(temp.path().join("existing.txt")).unwrap(),
            b"OLD-UNRELATED"
        );
        safe.write_atomic_new(Path::new("manifest"), b"FIRST-RECEIPT")
            .unwrap();
        assert!(safe
            .write_atomic_new(Path::new("manifest"), b"NO-OVERWRITE")
            .is_err());
        assert_eq!(
            std::fs::read(temp.path().join("manifest")).unwrap(),
            b"FIRST-RECEIPT"
        );
        assert!(safe.create_new_dir(Path::new("fresh")).is_ok());
        assert!(safe.create_new_dir(Path::new("fresh")).is_err());
    }
    #[test]
    fn moving_source_entries_refuses_occupied_targets() {
        let world = tempfile::tempdir().unwrap();
        let source = world.path().join("source");
        let archived = world.path().join("archived");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&archived).unwrap();
        std::fs::create_dir(source.join("project")).unwrap();
        std::fs::write(source.join("project/data"), b"SOURCE").unwrap();
        std::fs::write(source.join("file"), b"SOURCE").unwrap();
        std::fs::write(archived.join("file"), b"KEEP").unwrap();
        let source = SafeDir::open(&source, false).unwrap();
        let archived = SafeDir::open(&archived, false).unwrap();
        assert!(source
            .rename_entry_to(Path::new("file"), &archived, Path::new("file"))
            .is_err());
        assert_eq!(
            std::fs::read(world.path().join("archived/file")).unwrap(),
            b"KEEP"
        );
        source
            .rename_entry_to(Path::new("project"), &archived, Path::new("project"))
            .unwrap();
        assert_eq!(
            std::fs::read(world.path().join("archived/project/data")).unwrap(),
            b"SOURCE"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ordinary_directory_replacement_cannot_take_owned_payload_or_unknown_extras() {
        let world = tempfile::tempdir().unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let owned = root.create_new_dir(Path::new("project")).unwrap();
        std::fs::write(world.path().join("project/owned.txt"), b"OWNED").unwrap();
        std::fs::rename(world.path().join("project"), world.path().join("old-owned")).unwrap();
        std::fs::create_dir(world.path().join("project")).unwrap();
        std::fs::write(world.path().join("project/user-extra.txt"), b"UNKNOWN").unwrap();
        assert!(owned.require_current_binding().is_err());
        assert_eq!(
            std::fs::read(owned.anchored_path().join("owned.txt")).unwrap(),
            b"OWNED"
        );
        let source = world.path().join("new.txt");
        std::fs::write(&source, b"NEW").unwrap();
        assert!(owned
            .copy_new(&source, Path::new("new.txt"), false)
            .is_err());
        assert!(!world.path().join("project/new.txt").exists());
        assert_eq!(
            std::fs::read(world.path().join("project/user-extra.txt")).unwrap(),
            b"UNKNOWN"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn existing_and_late_parent_symlinks_never_receive_writes() {
        use std::os::unix::fs::symlink;
        let world = tempfile::tempdir().unwrap();
        let drive = world.path().join("drive");
        let outside = world.path().join("outside");
        std::fs::create_dir(&drive).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let safe = SafeDir::open(&drive, false).unwrap();
        symlink(&outside, drive.join("linked")).unwrap();
        assert!(safe.ensure_dir(Path::new("linked/payload")).is_err());
        assert!(!outside.join("payload").exists());
        let held = safe.create_new_dir(Path::new("held")).unwrap();
        std::fs::rename(drive.join("held"), drive.join("original")).unwrap();
        symlink(&outside, drive.join("held")).unwrap();
        assert!(held
            .write_atomic(Path::new("receipt.txt"), b"ANCHOR")
            .is_err());
        assert!(!drive.join("original/receipt.txt").exists());
        assert!(!outside.join("receipt.txt").exists());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn symlink_leaf_and_unique_temporary_collision_preserve_unrelated_data() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"KEEP").unwrap();
        symlink(&outside, temp.path().join("receipt")).unwrap();
        let safe = SafeDir::open(temp.path(), false).unwrap();
        assert!(safe.write_atomic(Path::new("receipt"), b"NO").is_err());
        assert!(safe.append_synced(Path::new("receipt"), b"NO").is_err());
        safe.append_synced(Path::new("append"), b"FIRST").unwrap();
        safe.append_synced(Path::new("append"), b"SECOND").unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("append")).unwrap(),
            b"FIRSTSECOND"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"KEEP");
        std::fs::write(temp.path().join("other.bftool-tmp"), b"UNKNOWN-TMP").unwrap();
        safe.write_atomic(Path::new("other"), b"OK").unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("other.bftool-tmp")).unwrap(),
            b"UNKNOWN-TMP"
        );
    }
}
