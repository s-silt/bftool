//! 统一操作请求（CLI/GUI → service::run）。

use std::path::PathBuf;

use crate::pipeline::archive::Options as ArchiveOptions;

/// 文件级后缀/glob 筛选（与 FolderProjects 可并存）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileFilter {
    /// 例如 zip / .zip / 7z
    pub extensions: Vec<String>,
    pub globs: Vec<String>,
    pub recursive: bool,
}

impl FileFilter {
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty() && self.globs.is_empty()
    }

    pub fn from_exts(exts: Vec<String>, recursive: bool) -> Self {
        Self {
            extensions: exts,
            globs: Vec::new(),
            recursive,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum SourceSpec {
    /// 现状：扫 Config.ready_root 下全部项目
    #[default]
    PendingRoot,
    /// FolderProjects；`files` 可叠加文件级后缀/glob
    Folder {
        root: PathBuf,
        include_subfolder_projects: bool,
        files: FileFilter,
    },
    /// 仅按文件后缀/glob 选择归档文件
    FileGlobs { root: PathBuf, files: FileFilter },
}

impl SourceSpec {
    pub fn folder(root: PathBuf) -> Self {
        Self::Folder {
            root,
            include_subfolder_projects: true,
            files: FileFilter::default(),
        }
    }

    pub fn folder_with_ext(root: PathBuf, exts: Vec<String>, recursive: bool) -> Self {
        Self::Folder {
            root,
            include_subfolder_projects: true,
            files: FileFilter::from_exts(exts, recursive),
        }
    }

    pub fn root_path<'a>(&'a self, ready_root: &'a std::path::Path) -> &'a std::path::Path {
        match self {
            Self::PendingRoot => ready_root,
            Self::Folder { root, .. } | Self::FileGlobs { root, .. } => root.as_path(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum OperationRequest {
    Init(InitRequest),
    Archive(ArchiveRequest),
    Watch(WatchRequest),
    Verify(VerifyRequest),
    Find(FindRequest),
    Drives,
    Status,
}

#[derive(Debug, Clone)]
pub struct InitRequest {
    pub drive_letter: String,
    pub id: Option<String>,
    pub force_system: bool,
    pub force_library: bool,
}

impl InitRequest {
    pub fn from_legacy_force(drive_letter: String, id: Option<String>, force: bool) -> Self {
        Self {
            drive_letter,
            id,
            force_system: force,
            force_library: force,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArchiveRequest {
    pub options: ArchiveOptions,
    pub source: SourceSpec,
    pub incremental: bool,
    pub retain_source: bool,
    /// 首轮可从全局 catalog 建立索引；不替代本轮 copy/verify
    pub seed_from_global_catalog: bool,
}

impl ArchiveRequest {
    pub fn from_options(options: ArchiveOptions) -> Self {
        Self {
            options,
            source: SourceSpec::PendingRoot,
            incremental: false,
            retain_source: false,
            seed_from_global_catalog: false,
        }
    }

    /// 合并进 Options，供 plan/run 使用。
    pub fn merged_options(&self) -> ArchiveOptions {
        let mut o = self.options.clone();
        o.incremental = self.incremental || o.incremental;
        o.retain_source = self.retain_source || o.retain_source;
        o.seed_from_global_catalog = self.seed_from_global_catalog || o.seed_from_global_catalog;
        match &self.source {
            SourceSpec::PendingRoot => {}
            SourceSpec::Folder {
                root,
                include_subfolder_projects,
                files,
            } => {
                o.source_override = Some(root.clone());
                o.include_subfolder_projects = *include_subfolder_projects;
                if !files.is_empty() {
                    o.include_ext = Some(files.extensions.clone());
                    o.file_globs = files.globs.clone();
                    o.ext_recursive = files.recursive;
                }
            }
            SourceSpec::FileGlobs { root, files } => {
                o.source_override = Some(root.clone());
                o.include_subfolder_projects = false;
                o.include_ext = Some(files.extensions.clone());
                o.file_globs = files.globs.clone();
                o.ext_recursive = files.recursive;
            }
        }
        o
    }
}

#[derive(Debug, Clone)]
pub struct WatchRequest {
    pub folder: PathBuf,
    pub files: FileFilter,
    pub poll_secs: u64,
    pub once: bool,
    pub archive: ArchiveOptions,
}

impl WatchRequest {
    pub fn to_archive_request(&self) -> ArchiveRequest {
        ArchiveRequest {
            options: self.archive.clone(),
            source: SourceSpec::Folder {
                root: self.folder.clone(),
                include_subfolder_projects: true,
                files: self.files.clone(),
            },
            incremental: true,
            retain_source: true,
            seed_from_global_catalog: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub drive: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FindRequest {
    pub keyword: String,
}

#[derive(Debug)]
pub enum OperationResult {
    Ok,
    Archive {
        summary: crate::pipeline::archive::ArchiveSummary,
    },
    Watch {
        cycles: u32,
        archived: usize,
        skipped_unchanged: usize,
        failed: usize,
    },
    Verify {
        report: crate::engine::verify::VerifyReport,
    },
    Find {
        outcome: crate::engine::find::FindOutcome,
    },
    HardFail {
        message: String,
    },
    Cancelled,
}
