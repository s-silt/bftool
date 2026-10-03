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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::reporter::ProgressHandle;
use sha2::{Digest, Sha256};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) struct SafeDir {
    path: PathBuf,
    handle: platform::Directory,
}

/// Identity of the filesystem object held by this file, independent of its name.
pub(crate) fn file_identity(file: &File) -> Result<String> {
    platform::file_identity(file).context("Read filesystem object identity failed")
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

    /// Identity of this exact open directory object, rather than its visible path.
    pub(crate) fn identity(&self) -> Result<String> {
        platform::directory_identity(&self.handle).context("Read open directory identity failed")
    }

    /// Open an existing ordinary child directory without following links or creating it.
    pub(crate) fn open_existing_dir(&self, relative: &Path) -> Result<Self> {
        self.require_current_binding()?;
        self.descend(relative, false, false)
    }

    /// Enumerate names only; callers must open each entry through no-follow handles.
    pub(crate) fn list_entries(&self) -> Result<Vec<OsString>> {
        self.list_entries_checked(|| Ok(()))
    }

    pub(crate) fn list_entries_checked(
        &self,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<Vec<OsString>> {
        check()?;
        self.require_current_binding()?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(self.anchored_path())? {
            check()?;
            entries.push(entry?.file_name());
        }
        check()?;
        self.require_current_binding()?;
        Ok(entries)
    }

    /// Hold a nonblocking OS lock on an ordinary job-owned lock file. Never unlink it.
    pub(crate) fn lock_exclusive(&self, relative: &Path) -> Result<File> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        let file = platform::lock_file(&parent.handle, &parent.path.join(&name), &name)
            .context("Acquire exclusive job lock failed")?;
        parent.require_current_binding()?;
        Ok(file)
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

    fn create_owned_temp(&self, target: &OsStr, suffix: &str) -> Result<File> {
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
            match platform::create_owned_file(&self.handle, &self.path.join(&name), &name) {
                Ok(file) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("Create owned direct-copy temporary failed")
                }
            }
        }
        bail!("Cannot create an exclusive direct-copy temporary; unknown entries preserved")
    }

    pub(crate) fn read_regular(&self, relative: &Path) -> Result<File> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        platform::read_file(&parent.handle, &parent.path.join(&name), &name)
            .with_context(|| format!("安全打开目标普通文件失败：{}", relative.display()))
    }

    /// Keep an ordinary source object's identity alive without locking out user
    /// edits or deletion. This attributes-only handle is not a source I/O handle.
    pub(crate) fn hold_file_identity(&self, relative: &Path) -> Result<File> {
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        let file = platform::hold_identity_file(&parent.handle, &parent.path.join(&name), &name)?;
        parent.require_current_binding()?;
        Ok(file)
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

    /// Direct-backup metadata is immutable. Retain the exact temporary object
    /// through no-replace publication or cleanup; never reuse its historical name.
    pub(crate) fn write_metadata_new(&self, relative: &Path, bytes: &[u8]) -> Result<String> {
        platform::require_owned_mutations()?;
        self.require_current_binding()?;
        let (parent, name) = self.parent_and_name(relative, false)?;
        let mut file = parent.create_owned_temp(&name, ".bftool-meta-tmp")?;
        let mut published = false;
        let result = (|| -> Result<String> {
            file.write_all(bytes)?;
            file.sync_all()?;
            #[cfg(test)]
            tests::METADATA_HOOK.with(|hook| {
                if let Some(callback) = hook.borrow_mut().take() {
                    callback(&file, &parent);
                }
            });
            parent.require_current_binding()?;
            self.require_current_binding()?;
            platform::publish_owned_file(&file, &parent.handle, &parent.path, &name)?;
            published = true;
            platform::sync_dir(&parent.handle)?;
            parent.require_current_binding()?;
            self.require_current_binding()?;
            file_identity(&file)
        })();
        if result.is_err() && !published {
            if let Err(cleanup) = platform::discard_owned_file(&file) {
                return result.with_context(|| {
                    format!("Owned metadata temporary cleanup failed; preserved: {cleanup}")
                });
            }
        }
        result.with_context(|| {
            format!(
                "Immutable metadata publication failed: {}",
                relative.display()
            )
        })
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

    /// Copy only from an already no-follow-opened ordinary source handle, rewinding
    /// it to byte zero. The caller owns the progress handle's finish lifecycle.
    pub(crate) fn copy_from_handle_new(
        &self,
        source: &mut File,
        relative: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn ProgressHandle,
    ) -> Result<String> {
        platform::require_owned_mutations()?;
        if cancel.load(Ordering::Acquire) {
            bail!("Copy cancelled; source retained");
        }
        self.require_current_binding()?;
        if !source.metadata()?.is_file() {
            bail!("Copy source handle is not an ordinary file");
        }
        source.seek(SeekFrom::Start(0))?;
        let (parent, name) = self.parent_and_name(relative, true)?;
        if cancel.load(Ordering::Acquire) {
            bail!("Copy cancelled; source retained");
        }
        let mut destination = parent.create_owned_temp(&name, crate::engine::cruft::PART_SUFFIX)?;
        let mut published = false;
        let result = (|| -> Result<String> {
            let mut hasher = Sha256::new();
            let mut buffer = vec![0; 1024 * 1024];
            loop {
                if cancel.load(Ordering::Acquire) {
                    bail!("Copy cancelled; source retained");
                }
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                if cancel.load(Ordering::Acquire) {
                    bail!("Copy cancelled; source retained");
                }
                destination.write_all(&buffer[..count])?;
                hasher.update(&buffer[..count]);
                progress.inc(count as u64);
            }
            if cancel.load(Ordering::Acquire) {
                bail!("Copy cancelled; source retained");
            }
            destination.sync_all()?;
            parent.require_current_binding()?;
            self.require_current_binding()?;
            if cancel.load(Ordering::Acquire) {
                bail!("Copy cancelled; source retained");
            }
            platform::publish_owned_file(&destination, &parent.handle, &parent.path, &name)?;
            published = true;
            platform::sync_dir(&parent.handle)?;
            parent.require_current_binding()?;
            self.require_current_binding()?;
            Ok(format!("{:X}", hasher.finalize()))
        })();
        if result.is_err() && !published {
            if let Err(cleanup) = platform::discard_owned_file(&destination) {
                drop(destination);
                return result.with_context(|| {
                    format!("Owned temporary cleanup failed; temporary preserved: {cleanup}")
                });
            }
        }
        drop(destination);
        result.with_context(|| format!("Safe handle copy failed: {}", relative.display()))
    }

    /// Publish only a known owned file/directory object, using its actual held
    /// handle after identity verification. Drop other handles to this payload
    /// and its descendants first; Windows denies obtaining DELETE access while
    /// existing handles omit delete sharing. Unsupported platforms fail closed.
    pub(crate) fn rename_owned_entry_to(
        &self,
        from: &Path,
        to_dir: &Self,
        to: &Path,
        expected_id: &str,
    ) -> Result<()> {
        platform::require_owned_mutations()?;
        self.require_current_binding()?;
        to_dir.require_current_binding()?;
        let (source_parent, source_name) = self.parent_and_name(from, false)?;
        let (destination_parent, destination_name) = to_dir.parent_and_name(to, false)?;
        let owned = platform::open_owned_entry(
            &source_parent.handle,
            &source_parent.path.join(&source_name),
            &source_name,
        )?;
        if file_identity(&owned)? != expected_id {
            bail!("Owned payload identity changed; unknown entry preserved");
        }
        source_parent.require_current_binding()?;
        destination_parent.require_current_binding()?;
        platform::publish_owned_file(
            &owned,
            &destination_parent.handle,
            &destination_parent.path,
            &destination_name,
        )?;
        platform::sync_dir(&source_parent.handle)?;
        platform::sync_dir(&destination_parent.handle)?;
        source_parent.require_current_binding()?;
        destination_parent.require_current_binding()?;
        Ok(())
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
    fn owned_mutation_unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "held-object direct copy/publication is implemented only on Windows; source retained",
        ))
    }
    pub(super) fn require_owned_mutations() -> io::Result<()> {
        owned_mutation_unsupported()
    }
    pub(super) fn create_owned_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        owned_mutation_unsupported()
    }
    pub(super) fn open_owned_entry(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        owned_mutation_unsupported()
    }
    pub(super) fn publish_owned_file(
        _: &File,
        _: &Directory,
        _: &Path,
        _: &OsStr,
    ) -> io::Result<()> {
        owned_mutation_unsupported()
    }
    pub(super) fn discard_owned_file(_: &File) -> io::Result<()> {
        owned_mutation_unsupported()
    }
    const O_RDONLY: i32 = 0;
    const O_WRONLY: i32 = 1;
    const O_RDWR: i32 = 2;
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
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
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
    pub(super) fn file_identity(file: &File) -> io::Result<String> {
        use std::os::unix::fs::MetadataExt;
        // File::metadata obtains fstat information from the held descriptor.
        let metadata = file.metadata()?;
        Ok(format!(
            "linux:{:016X}:{:016X}",
            metadata.dev(),
            metadata.ino()
        ))
    }
    pub(super) fn directory_identity(parent: &Directory) -> io::Result<String> {
        file_identity(&parent.0)
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
    pub(super) fn lock_file(parent: &Directory, _path: &Path, name: &OsStr) -> io::Result<File> {
        let file = open_relative(parent, name, O_RDWR | O_CREAT | O_NONBLOCK)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "job lock is not an ordinary file",
            ));
        }
        // SAFETY: file owns a live descriptor. flock is nonblocking, and the
        // exclusive lock remains held until this File is closed by its owner.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }
    pub(super) fn hold_identity_file(
        parent: &Directory,
        path: &Path,
        name: &OsStr,
    ) -> io::Result<File> {
        read_file(parent, path, name)
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
    use std::os::windows::io::AsRawHandle;
    use std::sync::Arc;

    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ: u32 = 1;
    const DELETE: u32 = 0x0001_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_RENAME_INFO: i32 = 3;
    const FILE_DISPOSITION_INFO: i32 = 4;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    pub(super) struct Directory {
        held: Vec<Arc<File>>,
    }
    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    // SDK 10.0.22621 winbase.h: the first union occupies a DWORD. For the
    // FileRenameInfo class its low BOOLEAN byte is ReplaceIfExists (always 0).
    #[repr(C)]
    struct FileRenameInfo {
        replace_if_exists: u32,
        root_directory: *mut std::ffi::c_void,
        file_name_length: u32,
        file_name: [u16; 1],
    }
    #[repr(C)]
    struct FileDispositionInfo {
        delete_file: u8,
    }
    pub(super) fn require_owned_mutations() -> io::Result<()> {
        Ok(())
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
    pub(super) fn file_identity(file: &File) -> io::Result<String> {
        let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::uninit();
        // SAFETY: File owns a live Windows handle; the repr(C) output buffer has
        // BY_HANDLE_FILE_INFORMATION's DWORD/FILETIME layout and size. The API
        // initializes it on success, which is checked before assume_init.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let information = unsafe { information.assume_init() };
        let index =
            ((information.file_index_high as u64) << 32) | information.file_index_low as u64;
        Ok(format!(
            "windows:{:08X}:{:016X}",
            information.volume_serial_number, index
        ))
    }
    pub(super) fn directory_identity(parent: &Directory) -> io::Result<String> {
        let file = parent.held.last().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "directory handle is missing")
        })?;
        file_identity(file)
    }
    pub(super) fn same_dir(a: &Directory, b: &Directory) -> io::Result<bool> {
        Ok(directory_identity(a)? == directory_identity(b)?)
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
    pub(super) fn create_owned_file(
        _parent: &Directory,
        path: &Path,
        _name: &OsStr,
    ) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES)
            .create_new(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        ordinary(&file, false)?;
        Ok(file)
    }
    pub(super) fn open_owned_entry(
        _parent: &Directory,
        path: &Path,
        _name: &OsStr,
    ) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .access_mode(DELETE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let metadata = file.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (!metadata.is_file() && !metadata.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned entry is a reparse point or special entry",
            ));
        }
        Ok(file)
    }
    pub(super) fn publish_owned_file(
        file: &File,
        _parent: &Directory,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        let name: Vec<u16> = parent_path.join(name).as_os_str().encode_wide().collect();
        if name.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in owned rename target",
            ));
        }
        let name_bytes = name
            .len()
            .checked_mul(2)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "owned rename target too long")
            })?;
        let name_offset = std::mem::offset_of!(FileRenameInfo, file_name);
        let buffer_bytes = name_offset
            .checked_add(name_bytes as usize)
            .and_then(|bytes| bytes.checked_add(2))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "owned rename buffer too long")
            })?
            .max(std::mem::size_of::<FileRenameInfo>());
        let buffer_size = u32::try_from(buffer_bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "owned rename buffer too long")
        })?;
        // usize storage supplies HANDLE alignment on both 32-bit and 64-bit
        // Windows. FileName is the SDK's trailing variable UTF-16 array.
        let mut buffer = vec![0usize; buffer_bytes.div_ceil(std::mem::size_of::<usize>())];
        let information = buffer.as_mut_ptr().cast::<FileRenameInfo>();
        // SAFETY: aligned zeroed storage is large enough for the repr(C) header
        // and the entire UTF-16 name plus NUL. The ancestors represented by
        // parent_path stay held without delete sharing, so target resolution is
        // anchored. Only the source File handle is authoritative for the rename.
        unsafe {
            (*information).replace_if_exists = 0;
            (*information).root_directory = std::ptr::null_mut();
            (*information).file_name_length = name_bytes;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                std::ptr::addr_of_mut!((*information).file_name).cast::<u16>(),
                name.len(),
            );
        }
        // SAFETY: File owns a live DELETE-capable handle; the live buffer is
        // FILE_RENAME_INFO with ReplaceIfExists FALSE, hence atomic no-replace.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FILE_RENAME_INFO,
                information.cast(),
                buffer_size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub(super) fn discard_owned_file(file: &File) -> io::Result<()> {
        let mut information = FileDispositionInfo { delete_file: 1 };
        // SAFETY: File owns a live DELETE-capable handle; the one-byte repr(C)
        // FILE_DISPOSITION_INFO marks that held object for deletion on close.
        // No historical pathname is inspected, renamed, or unlinked.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FILE_DISPOSITION_INFO,
                (&mut information as *mut FileDispositionInfo).cast(),
                std::mem::size_of::<FileDispositionInfo>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
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
    pub(super) fn lock_file(_parent: &Directory, path: &Path, _name: &OsStr) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        ordinary(&file, false)?;
        Ok(file)
    }
    pub(super) fn hold_identity_file(
        _parent: &Directory,
        path: &Path,
        _name: &OsStr,
    ) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | 2 | 4)
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
        fn SetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            information_class: i32,
            information: *mut std::ffi::c_void,
            buffer_size: u32,
        ) -> i32;
        fn GetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;
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
    fn owned_mutation_unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "held-object direct copy/publication is implemented only on Windows; source retained",
        ))
    }
    pub(super) fn require_owned_mutations() -> io::Result<()> {
        owned_mutation_unsupported()
    }
    pub(super) fn create_owned_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        owned_mutation_unsupported()
    }
    pub(super) fn open_owned_entry(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        owned_mutation_unsupported()
    }
    pub(super) fn publish_owned_file(
        _: &File,
        _: &Directory,
        _: &Path,
        _: &OsStr,
    ) -> io::Result<()> {
        owned_mutation_unsupported()
    }
    pub(super) fn discard_owned_file(_: &File) -> io::Result<()> {
        owned_mutation_unsupported()
    }
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
    pub(super) fn file_identity(_: &File) -> io::Result<String> {
        unsupported()
    }
    pub(super) fn directory_identity(_: &Directory) -> io::Result<String> {
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
    pub(super) fn lock_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub(super) fn hold_identity_file(_: &Directory, _: &Path, _: &OsStr) -> io::Result<File> {
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
    use crate::reporter::ProgressHandle;
    use std::sync::atomic::AtomicBool;

    // Runs only on this test thread, after a metadata temporary has been synced.
    type MetadataHook = Box<dyn FnOnce(&File, &SafeDir)>;
    thread_local! {
        pub(super) static METADATA_HOOK: std::cell::RefCell<Option<MetadataHook>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(windows)]
    #[test]
    fn final_fix_metadata_held_publication_preserves_unknown_temp_name() {
        metadata_temp_substitution(false);
    }

    #[cfg(windows)]
    #[test]
    fn final_fix_metadata_held_cleanup_preserves_unknown_temp_name_and_destination() {
        metadata_temp_substitution(true);
    }

    #[cfg(windows)]
    fn metadata_temp_substitution(occupied: bool) {
        let world = tempfile::tempdir().unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        if occupied {
            std::fs::write(world.path().join("record.json"), b"DESTINATION-KEEP").unwrap();
        }
        let historical = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
        let capture = historical.clone();
        METADATA_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |held, dir| {
                let name = dir
                    .list_entries()
                    .unwrap()
                    .into_iter()
                    .find(|n| n.to_string_lossy().ends_with(".bftool-meta-tmp"))
                    .unwrap();
                let path = dir.path.join(&name);
                // Stronger than an external rename (blocked by sharing): move through
                // the exact held object and place unrelated bytes at its old name.
                platform::publish_owned_file(
                    held,
                    &dir.handle,
                    &dir.path,
                    OsStr::new("saved-owned"),
                )
                .unwrap();
                std::fs::write(&path, b"TEMP-KEEP").unwrap();
                *capture.lock().unwrap() = path;
            }))
        });
        let result = root.write_metadata_new(Path::new("record.json"), b"OWNED-METADATA");
        assert_eq!(result.is_err(), occupied);
        assert_eq!(
            std::fs::read(&*historical.lock().unwrap()).unwrap(),
            b"TEMP-KEEP"
        );
        assert_eq!(
            std::fs::read(world.path().join("record.json")).unwrap(),
            if occupied {
                b"DESTINATION-KEEP".as_slice()
            } else {
                b"OWNED-METADATA".as_slice()
            }
        );
        assert!(!world.path().join("saved-owned").exists());
    }

    struct CopyProgress<'a> {
        bytes: u64,
        cancel_after_chunk: Option<&'a AtomicBool>,
    }

    impl ProgressHandle for CopyProgress<'_> {
        fn inc(&mut self, delta: u64) {
            self.bytes += delta;
            if let Some(cancel) = self.cancel_after_chunk {
                cancel.store(true, Ordering::Release);
            }
        }

        fn finish(&mut self) {}
    }

    #[test]
    fn direct_io_identity_hold_allows_source_edits_without_changing_object_identity() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("source"), b"BEFORE").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = root.hold_file_identity(Path::new("source")).unwrap();
        let identity = file_identity(&held).unwrap();
        std::fs::write(world.path().join("source"), b"AFTER-USER-EDIT").unwrap();
        assert_eq!(file_identity(&held).unwrap(), identity);
        assert_eq!(
            std::fs::read(world.path().join("source")).unwrap(),
            b"AFTER-USER-EDIT"
        );
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_r1_held_publish_preserves_unknown_historical_name_and_hard_link() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("owned"), b"abc").unwrap();
        std::fs::hard_link(world.path().join("owned"), world.path().join("alias")).unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = platform::open_owned_entry(
            &root.handle,
            &world.path().join("owned"),
            OsStr::new("owned"),
        )
        .unwrap();
        let original_identity = file_identity(&held).unwrap();
        platform::publish_owned_file(&held, &root.handle, world.path(), OsStr::new("saved-owned"))
            .unwrap();
        std::fs::write(world.path().join("owned"), b"UNKNOWN").unwrap();
        platform::publish_owned_file(&held, &root.handle, world.path(), OsStr::new("published"))
            .unwrap();
        assert_eq!(file_identity(&held).unwrap(), original_identity);
        drop(held);
        assert_eq!(
            std::fs::read(world.path().join("published")).unwrap(),
            b"abc"
        );
        assert_eq!(
            std::fs::read(world.path().join("owned")).unwrap(),
            b"UNKNOWN"
        );
        assert_eq!(std::fs::read(world.path().join("alias")).unwrap(), b"abc");
        assert!(!world.path().join("saved-owned").exists());
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_r1_held_cleanup_preserves_unknown_historical_name_and_hard_link() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("owned"), b"abc").unwrap();
        std::fs::hard_link(world.path().join("owned"), world.path().join("alias")).unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = platform::open_owned_entry(
            &root.handle,
            &world.path().join("owned"),
            OsStr::new("owned"),
        )
        .unwrap();
        platform::publish_owned_file(&held, &root.handle, world.path(), OsStr::new("saved-owned"))
            .unwrap();
        std::fs::write(world.path().join("owned"), b"UNKNOWN").unwrap();
        platform::discard_owned_file(&held).unwrap();
        drop(held);
        assert!(!world.path().join("saved-owned").exists());
        assert_eq!(
            std::fs::read(world.path().join("owned")).unwrap(),
            b"UNKNOWN"
        );
        assert_eq!(std::fs::read(world.path().join("alias")).unwrap(), b"abc");
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_r1_held_publish_refuses_occupied_target() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("occupied"), b"KEEP").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let mut held = platform::create_owned_file(
            &root.handle,
            &world.path().join("owned"),
            OsStr::new("owned"),
        )
        .unwrap();
        held.write_all(b"abc").unwrap();
        held.sync_all().unwrap();
        assert!(platform::publish_owned_file(
            &held,
            &root.handle,
            world.path(),
            OsStr::new("occupied")
        )
        .is_err());
        drop(held);
        assert_eq!(
            std::fs::read(world.path().join("occupied")).unwrap(),
            b"KEEP"
        );
        assert_eq!(std::fs::read(world.path().join("owned")).unwrap(), b"abc");
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_r1_owned_publish_rejects_replaced_file_and_preserves_unknown() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("owned"), b"SOURCE").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = root.read_regular(Path::new("owned")).unwrap();
        let expected_identity = file_identity(&held).unwrap();
        drop(held);
        std::fs::rename(world.path().join("owned"), world.path().join("saved-owned")).unwrap();
        std::fs::write(world.path().join("owned"), b"UNKNOWN").unwrap();
        assert!(root
            .rename_owned_entry_to(
                Path::new("owned"),
                &root,
                Path::new("published"),
                &expected_identity
            )
            .is_err());
        assert!(!world.path().join("published").exists());
        assert_eq!(
            std::fs::read(world.path().join("owned")).unwrap(),
            b"UNKNOWN"
        );
        assert_eq!(
            std::fs::read(world.path().join("saved-owned")).unwrap(),
            b"SOURCE"
        );
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_r1_owned_publish_moves_expected_file_without_replacing() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("owned"), b"SOURCE").unwrap();
        std::fs::write(world.path().join("occupied"), b"KEEP").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = root.read_regular(Path::new("owned")).unwrap();
        let expected_identity = file_identity(&held).unwrap();
        drop(held);
        assert!(root
            .rename_owned_entry_to(
                Path::new("owned"),
                &root,
                Path::new("occupied"),
                &expected_identity
            )
            .is_err());
        assert_eq!(
            std::fs::read(world.path().join("owned")).unwrap(),
            b"SOURCE"
        );
        assert_eq!(
            std::fs::read(world.path().join("occupied")).unwrap(),
            b"KEEP"
        );
        root.rename_owned_entry_to(
            Path::new("owned"),
            &root,
            Path::new("published"),
            &expected_identity,
        )
        .unwrap();
        assert!(!world.path().join("owned").exists());
        assert_eq!(
            std::fs::read(world.path().join("published")).unwrap(),
            b"SOURCE"
        );
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_r1_owned_publish_moves_expected_directory_after_handles_drop() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("owned")).unwrap();
        std::fs::write(world.path().join("owned/data"), b"SOURCE").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = root.open_existing_dir(Path::new("owned")).unwrap();
        let expected_identity = held.identity().unwrap();
        drop(held);
        root.rename_owned_entry_to(
            Path::new("owned"),
            &root,
            Path::new("published"),
            &expected_identity,
        )
        .unwrap();
        assert!(!world.path().join("owned").exists());
        assert_eq!(
            std::fs::read(world.path().join("published/data")).unwrap(),
            b"SOURCE"
        );
    }

    #[test]
    #[ignore = "child-process helper, invoked by direct_io_lock_excludes_other_processes"]
    fn direct_io_lock_child() {
        let fixture = std::env::var_os("BFTOOL_DIRECT_IO_LOCK_FIXTURE").unwrap();
        let root = SafeDir::open(Path::new(&fixture), false).unwrap();
        let result = root.lock_exclusive(Path::new("lock"));
        if std::env::var_os("BFTOOL_DIRECT_IO_LOCK_EXPECT_ACQUIRE").is_some() {
            assert!(
                result.is_ok(),
                "released lock must be acquirable in a child process"
            );
        } else {
            assert!(result.is_err(), "held lock must exclude a child process");
        }
    }

    #[test]
    fn direct_io_lock_excludes_other_processes_and_releases_on_drop() {
        fn run_child(fixture: &Path, expect_acquire: bool) {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "engine::destination::tests::direct_io_lock_child",
                "--ignored",
                "--nocapture",
            ]);
            command.env("BFTOOL_DIRECT_IO_LOCK_FIXTURE", fixture);
            command.env_remove("BFTOOL_DIRECT_IO_LOCK_EXPECT_ACQUIRE");
            if expect_acquire {
                command.env("BFTOOL_DIRECT_IO_LOCK_EXPECT_ACQUIRE", "1");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "child lock check failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let world = tempfile::tempdir().unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = root.lock_exclusive(Path::new("lock")).unwrap();
        run_child(world.path(), false);
        drop(held);
        run_child(world.path(), true);
        assert!(world.path().join("lock").is_file());
    }

    #[test]
    fn direct_io_exclusive_lock_refuses_another_handle_without_changing_contents() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("lock"), b"OWNED-LOCK").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = root.lock_exclusive(Path::new("lock")).unwrap();
        assert!(root.lock_exclusive(Path::new("lock")).is_err());
        drop(held);
        assert_eq!(
            std::fs::read(world.path().join("lock")).unwrap(),
            b"OWNED-LOCK"
        );
    }

    #[test]
    fn direct_io_exclusive_lock_can_be_reopened_after_held_handle_drops() {
        let world = tempfile::tempdir().unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let held = root.lock_exclusive(Path::new("lock")).unwrap();
        assert!(root.lock_exclusive(Path::new("lock")).is_err());
        drop(held);
        let reopened = root.lock_exclusive(Path::new("lock")).unwrap();
        assert!(root.lock_exclusive(Path::new("lock")).is_err());
        drop(reopened);
        assert!(world.path().join("lock").is_file());
    }

    #[test]
    fn direct_io_directory_comparison_distinguishes_unrelated_objects() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("a")).unwrap();
        std::fs::create_dir(world.path().join("b")).unwrap();
        let a = SafeDir::open(&world.path().join("a"), false).unwrap();
        let reopened = SafeDir::open(&world.path().join("a"), false).unwrap();
        let b = SafeDir::open(&world.path().join("b"), false).unwrap();
        assert!(platform::same_dir(&a.handle, &reopened.handle).unwrap());
        assert!(!platform::same_dir(&a.handle, &b.handle).unwrap());
    }

    #[test]
    fn direct_io_identity_is_stable_for_handles_and_hard_links() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("child")).unwrap();
        std::fs::write(world.path().join("source"), b"abc").unwrap();
        std::fs::hard_link(world.path().join("source"), world.path().join("alias")).unwrap();
        std::fs::write(world.path().join("unrelated"), b"abc").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let reopened = SafeDir::open(world.path(), false).unwrap();
        let child = root.open_existing_dir(Path::new("child")).unwrap();
        assert_eq!(root.identity().unwrap(), reopened.identity().unwrap());
        assert_ne!(root.identity().unwrap(), child.identity().unwrap());
        let source = root.read_regular(Path::new("source")).unwrap();
        let alias = root.read_regular(Path::new("alias")).unwrap();
        let unrelated = root.read_regular(Path::new("unrelated")).unwrap();
        assert_eq!(
            file_identity(&source).unwrap(),
            file_identity(&alias).unwrap()
        );
        assert_ne!(
            file_identity(&source).unwrap(),
            file_identity(&unrelated).unwrap()
        );
        assert_eq!(
            file_identity(&source).unwrap(),
            file_identity(&source).unwrap()
        );
    }

    #[test]
    fn direct_io_existing_directory_open_never_creates_or_accepts_files() {
        let world = tempfile::tempdir().unwrap();
        std::fs::write(world.path().join("file"), b"KEEP").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        assert!(root.open_existing_dir(Path::new("missing/nested")).is_err());
        assert!(!world.path().join("missing").exists());
        assert!(root.open_existing_dir(Path::new("file")).is_err());
        assert!(root.open_existing_dir(Path::new("../outside")).is_err());
    }

    #[test]
    fn direct_io_enumeration_returns_child_names_without_creating_entries() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("child")).unwrap();
        std::fs::write(world.path().join("file"), b"KEEP").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let mut names = root.list_entries().unwrap();
        names.sort();
        assert_eq!(names, vec![OsString::from("child"), OsString::from("file")]);
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_handle_copy_rewinds_hashes_and_retains_source() {
        use std::io::{Seek, SeekFrom};
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("destination")).unwrap();
        std::fs::write(world.path().join("source"), b"abc").unwrap();
        let source_root = SafeDir::open(world.path(), false).unwrap();
        let destination = source_root
            .open_existing_dir(Path::new("destination"))
            .unwrap();
        let mut source = source_root.read_regular(Path::new("source")).unwrap();
        source.seek(SeekFrom::Start(1)).unwrap();
        let cancel = AtomicBool::new(false);
        let mut progress = CopyProgress {
            bytes: 0,
            cancel_after_chunk: None,
        };
        let digest = destination
            .copy_from_handle_new(
                &mut source,
                Path::new("nested/copied"),
                &cancel,
                &mut progress,
            )
            .unwrap();
        assert_eq!(
            digest,
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
        );
        assert_eq!(
            std::fs::read(world.path().join("destination/nested/copied")).unwrap(),
            b"abc"
        );
        assert_eq!(std::fs::read(world.path().join("source")).unwrap(), b"abc");
        assert_eq!(progress.bytes, 3);
        let mut names = destination
            .open_existing_dir(Path::new("nested"))
            .unwrap()
            .list_entries()
            .unwrap();
        names.sort();
        assert_eq!(names, vec![OsString::from("copied")]);
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_handle_copy_refuses_existing_target_and_keeps_unknown_temporary() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("destination")).unwrap();
        std::fs::write(world.path().join("source"), b"NEW").unwrap();
        std::fs::write(world.path().join("destination/occupied"), b"KEEP").unwrap();
        std::fs::write(
            world.path().join("destination/unknown.bftool-part"),
            b"UNKNOWN",
        )
        .unwrap();
        let source_root = SafeDir::open(world.path(), false).unwrap();
        let destination = source_root
            .open_existing_dir(Path::new("destination"))
            .unwrap();
        let mut source = source_root.read_regular(Path::new("source")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut progress = CopyProgress {
            bytes: 0,
            cancel_after_chunk: None,
        };
        assert!(destination
            .copy_from_handle_new(&mut source, Path::new("occupied"), &cancel, &mut progress,)
            .is_err());
        assert_eq!(
            std::fs::read(world.path().join("destination/occupied")).unwrap(),
            b"KEEP"
        );
        assert_eq!(std::fs::read(world.path().join("source")).unwrap(), b"NEW");
        assert_eq!(
            std::fs::read(world.path().join("destination/unknown.bftool-part")).unwrap(),
            b"UNKNOWN"
        );
        let mut names = destination.list_entries().unwrap();
        names.sort();
        assert_eq!(
            names,
            vec![
                OsString::from("occupied"),
                OsString::from("unknown.bftool-part")
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn direct_io_cancelled_handle_copy_never_publishes_or_leaves_temporary_files() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("destination")).unwrap();
        let payload = vec![b'x'; 2 * 1024 * 1024 + 1];
        std::fs::write(world.path().join("source"), &payload).unwrap();
        let source_root = SafeDir::open(world.path(), false).unwrap();
        let destination = source_root
            .open_existing_dir(Path::new("destination"))
            .unwrap();
        let mut source = source_root.read_regular(Path::new("source")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut progress = CopyProgress {
            bytes: 0,
            cancel_after_chunk: Some(&cancel),
        };
        assert!(destination
            .copy_from_handle_new(&mut source, Path::new("cancelled"), &cancel, &mut progress,)
            .is_err());
        assert!(destination.list_entries().unwrap().is_empty());
        assert_eq!(std::fs::read(world.path().join("source")).unwrap(), payload);
        assert!(progress.bytes > 0 && progress.bytes < payload.len() as u64);
    }

    #[test]
    fn direct_io_pre_cancelled_handle_copy_does_not_create_parent_directories() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("destination")).unwrap();
        std::fs::write(world.path().join("source"), b"abc").unwrap();
        let source_root = SafeDir::open(world.path(), false).unwrap();
        let destination = source_root
            .open_existing_dir(Path::new("destination"))
            .unwrap();
        let mut source = source_root.read_regular(Path::new("source")).unwrap();
        let cancel = AtomicBool::new(true);
        let mut progress = CopyProgress {
            bytes: 0,
            cancel_after_chunk: None,
        };
        assert!(destination
            .copy_from_handle_new(
                &mut source,
                Path::new("missing/copied"),
                &cancel,
                &mut progress,
            )
            .is_err());
        assert!(destination.list_entries().unwrap().is_empty());
        assert_eq!(progress.bytes, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "new direct copy fails closed on Linux until held-object mutation exists"]
    fn direct_io_handle_copy_reads_held_source_after_visible_name_changes() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("destination")).unwrap();
        std::fs::write(world.path().join("source"), b"abc").unwrap();
        let source_root = SafeDir::open(world.path(), false).unwrap();
        let destination = source_root
            .open_existing_dir(Path::new("destination"))
            .unwrap();
        let mut source = source_root.read_regular(Path::new("source")).unwrap();
        std::fs::rename(
            world.path().join("source"),
            world.path().join("held-source"),
        )
        .unwrap();
        std::fs::write(world.path().join("source"), b"DECOY").unwrap();
        let cancel = AtomicBool::new(false);
        let mut progress = CopyProgress {
            bytes: 0,
            cancel_after_chunk: None,
        };
        let digest = destination
            .copy_from_handle_new(&mut source, Path::new("copied"), &cancel, &mut progress)
            .unwrap();
        assert_eq!(
            digest,
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
        );
        assert_eq!(
            std::fs::read(world.path().join("destination/copied")).unwrap(),
            b"abc"
        );
        assert_eq!(
            std::fs::read(world.path().join("held-source")).unwrap(),
            b"abc"
        );
        assert_eq!(
            std::fs::read(world.path().join("source")).unwrap(),
            b"DECOY"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_io_linux_owned_mutations_fail_closed_without_touching_files_or_names() {
        let world = tempfile::tempdir().unwrap();
        std::fs::create_dir(world.path().join("destination")).unwrap();
        std::fs::write(world.path().join("source"), b"abc").unwrap();
        let root = SafeDir::open(world.path(), false).unwrap();
        let destination = root.open_existing_dir(Path::new("destination")).unwrap();
        let mut source = root.read_regular(Path::new("source")).unwrap();
        source.seek(SeekFrom::Start(1)).unwrap();
        let cancel = AtomicBool::new(false);
        let mut progress = CopyProgress {
            bytes: 0,
            cancel_after_chunk: None,
        };
        let error = destination
            .copy_from_handle_new(
                &mut source,
                Path::new("missing/copied"),
                &cancel,
                &mut progress,
            )
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_eq!(source.stream_position().unwrap(), 1);
        assert!(destination.list_entries().unwrap().is_empty());
        let error = root
            .rename_owned_entry_to(
                Path::new("source"),
                &destination,
                Path::new("copied"),
                &file_identity(&source).unwrap(),
            )
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert!(destination.list_entries().unwrap().is_empty());
        assert_eq!(std::fs::read(world.path().join("source")).unwrap(), b"abc");
        assert_eq!(progress.bytes, 0);
    }

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
