//! GUI dependency boundary. Demo never delegates to production I/O.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use bftool_core::config::{Config, ConfigSource, LoadedConfig, SaveTarget};
use bftool_core::engine::archive::{ArchivePlan, ArchiveSummary, Options, PlanAction, PlanItem};
use bftool_core::engine::drive::{DriveInfo, InitCandidate};
use bftool_core::engine::find::{FindMatch, FindOutcome};
use bftool_core::engine::status::{DriveStatus, StatusReport};
use bftool_core::engine::verify::{ExtraFile, VerifyOutcome, VerifyReport};
use bftool_core::engine::verify_state::LastVerify;
use bftool_core::pipeline::watch::WatchSummary;
use bftool_core::reporter::Reporter;
use bftool_core::service::{self, WatchRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Live,
    Demo,
}

#[derive(Debug, Clone)]
pub struct OfflineDriveInfo {
    pub id: String,
    pub last_verify_when: String,
    pub last_verify_status: VerifyOutcome,
}

impl Backend {
    pub fn is_demo(self) -> bool {
        self == Self::Demo
    }

    pub fn load_config(self) -> anyhow::Result<LoadedConfig> {
        match self {
            Self::Live => Config::load_with_source(None),
            Self::Demo => Ok(LoadedConfig {
                config: synthetic_config(),
                source: ConfigSource::Default,
            }),
        }
    }

    pub fn status(self, cfg: &Config) -> anyhow::Result<StatusReport> {
        match self {
            Self::Live => {
                bftool_core::engine::status::gather(cfg, &bftool_core::reporter::NoopReporter)
            }
            Self::Demo => Ok(StatusReport {
                ready_root: cfg.ready_root.clone(),
                archived_root: cfg.archived_root.clone(),
                system_root: cfg.system_root.clone(),
                reserve_gb: cfg.reserve_gb,
                stable_minutes: cfg.stable_minutes,
                min_drive_gb: cfg.min_drive_gb,
                drives: demo_drives()
                    .into_iter()
                    .enumerate()
                    .map(|(i, drive)| DriveStatus {
                        last_verify: Some(LastVerify {
                            drive_id: drive.id.clone(),
                            when: format!("2026-10-0{}T10:15:00Z", 2 - i),
                            status: VerifyOutcome::Clean,
                        }),
                        drive,
                    })
                    .collect(),
                pending_count: 3,
                txn_pending: false,
            }),
        }
    }

    pub fn drives(self) -> anyhow::Result<Vec<DriveInfo>> {
        match self {
            Self::Live => service::scan_drives(None),
            Self::Demo => Ok(demo_drives()),
        }
    }

    pub fn init_candidates(self, cfg: &Config) -> anyhow::Result<Vec<InitCandidate>> {
        match self {
            Self::Live => service::list_init_candidates(cfg),
            Self::Demo => Ok(vec![
                InitCandidate {
                    letter: "DEMO-G".into(),
                    total_gb: 931,
                    is_system: false,
                    is_library: false,
                    already_backup: false,
                    non_empty: false,
                    can_init: true,
                    block_reason: None,
                    soft_hint: None,
                },
                InitCandidate {
                    letter: "DEMO-H".into(),
                    total_gb: 1863,
                    is_system: false,
                    is_library: false,
                    already_backup: false,
                    non_empty: true,
                    can_init: true,
                    block_reason: None,
                    soft_hint: Some(
                        "【演示】目录含文件，模拟初始化保留现有内容，不创建真实目录或标识".into(),
                    ),
                },
                InitCandidate {
                    letter: "DEMO-C".into(),
                    total_gb: 476,
                    is_system: true,
                    is_library: false,
                    already_backup: false,
                    non_empty: true,
                    can_init: false,
                    block_reason: Some("演示受保护盘，禁止生产操作".into()),
                    soft_hint: None,
                },
            ]),
        }
    }

    pub fn last_verify(self, cfg: &Config, id: &str) -> anyhow::Result<Option<LastVerify>> {
        match self {
            Self::Live => bftool_core::engine::verify_state::read_last_verify(&cfg.system_root, id),
            Self::Demo => Ok(self
                .status(cfg)?
                .drives
                .into_iter()
                .find(|d| d.drive.id == id)
                .and_then(|d| d.last_verify)),
        }
    }

    pub fn offline_drives(self, cfg: &Config, online: &[DriveInfo]) -> Vec<OfflineDriveInfo> {
        if self.is_demo() {
            return Vec::new();
        }
        let online_ids: HashSet<_> = online.iter().map(|d| d.id.as_str()).collect();
        let path = bftool_core::engine::paths::system_verify_log(&cfg.system_root);
        let mut result = Vec::new();
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let parts: Vec<_> = line.split(',').map(str::trim).collect();
                if parts.len() < 3
                    || online_ids.contains(parts[0])
                    || result.iter().any(|d: &OfflineDriveInfo| d.id == parts[0])
                {
                    continue;
                }
                let number = |i: usize| parts.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                let status = match parts[2] {
                    "Clean" => VerifyOutcome::Clean,
                    "IssuesFound" => VerifyOutcome::IssuesFound { bad: number(3) },
                    "ExtraOnly" => VerifyOutcome::ExtraOnly { extra: number(4) },
                    "CleanButSizeOnly" => VerifyOutcome::CleanButSizeOnly {
                        size_only: number(5),
                    },
                    _ => VerifyOutcome::Cancelled,
                };
                result.push(OfflineDriveInfo {
                    id: parts[0].into(),
                    last_verify_when: parts[1].into(),
                    last_verify_status: status,
                });
            }
        }
        result
    }

    pub fn plan(
        self,
        cfg: &Config,
        opts: &Options,
        reporter: &dyn Reporter,
    ) -> anyhow::Result<ArchivePlan> {
        match self {
            Self::Live => service::plan_archive(cfg, opts, reporter),
            Self::Demo => {
                reporter.info("【演示合成】从内存生成计划，不访问目录或磁盘");
                Ok(demo_plan(cfg, opts))
            }
        }
    }

    pub fn archive(
        self,
        cfg: &Config,
        plan: &ArchivePlan,
        cancel: &AtomicBool,
        reporter: &dyn Reporter,
    ) -> anyhow::Result<ArchiveSummary> {
        match self {
            Self::Live => service::run_archive_plan(cfg, plan, cancel, reporter),
            Self::Demo => {
                let cancelled = simulate(cancel, reporter);
                Ok(ArchiveSummary {
                    handled: if cancelled {
                        0
                    } else {
                        plan.items
                            .iter()
                            .filter(|p| {
                                matches!(
                                    p.action,
                                    PlanAction::Archive { .. }
                                        | PlanAction::RenameAndArchive { .. }
                                )
                            })
                            .count()
                    },
                    failed: 0,
                    cancelled,
                    sealed_stopped: false,
                })
            }
        }
    }

    pub fn verify(
        self,
        cfg: &Config,
        selected: Option<&str>,
        target: Option<&Path>,
        cancel: &AtomicBool,
        reporter: &dyn Reporter,
    ) -> anyhow::Result<VerifyReport> {
        match self {
            Self::Live => match target {
                Some(p) => service::verify_one(cfg, p, cancel, reporter),
                None => service::run_verify(cfg, selected, cancel, reporter),
            },
            Self::Demo => {
                let cancelled = simulate(cancel, reporter);
                Ok(VerifyReport {
                    checked: if cancelled { 0 } else { 152 },
                    extra: if cancelled { 0 } else { 1 },
                    extras: if cancelled {
                        Vec::new()
                    } else {
                        vec![ExtraFile {
                            project: "演示纪录片".into(),
                            rel: "演示额外文件.txt".into(),
                        }]
                    },
                    cancelled,
                    ..Default::default()
                })
            }
        }
    }

    pub fn find(self, cfg: &Config, keyword: &str) -> anyhow::Result<FindOutcome> {
        match self {
            Self::Live => service::find_search(cfg, keyword),
            Self::Demo => Ok(FindOutcome {
                matches: [
                    "2026秋季高清纪录片合集_主拍摄机位A",
                    "2025自然风光纪录片预告片_4K母带",
                ]
                .into_iter()
                .enumerate()
                .filter(|(_, name)| name.to_lowercase().contains(&keyword.to_lowercase()))
                .map(|(i, name)| FindMatch {
                    folder: name.into(),
                    drive_id: format!("演示盘{}", i + 1),
                    archived_time: "2026-10-02 14:20:11".into(),
                    project_no: format!("00{}", i + 1),
                    in_drive_path: format!("【演示】/项目/00{}_{name}", i + 1),
                    verify: "【演示】SHA256-OK".into(),
                    source: "【演示】内存索引".into(),
                })
                .collect(),
                sources_searched: 2,
                sources_failed: Vec::new(),
                malformed_rows: 0,
            }),
        }
    }

    pub fn initialize(
        self,
        cfg: &Config,
        letter: &str,
        id: Option<&str>,
        force: bool,
        reporter: &dyn Reporter,
    ) -> anyhow::Result<()> {
        match self {
            Self::Live => service::run_init_legacy(cfg, letter, id, force, reporter),
            Self::Demo => {
                reporter.info("【演示合成】仅模拟初始化，没有创建目录或写入磁盘");
                Ok(())
            }
        }
    }

    pub fn watch_folder_exists(self, folder: &Path) -> bool {
        match self {
            Self::Live => folder.exists(),
            Self::Demo => !folder.as_os_str().is_empty(),
        }
    }

    pub fn watch(
        self,
        cfg: &Config,
        req: &WatchRequest,
        cancel: &AtomicBool,
        reporter: &dyn Reporter,
    ) -> anyhow::Result<WatchSummary> {
        match self {
            Self::Live => bftool_core::pipeline::watch::run(cfg, req, reporter, cancel),
            Self::Demo => {
                let cancelled = simulate(cancel, reporter);
                Ok(WatchSummary {
                    cycles: 1,
                    archived: usize::from(!cancelled),
                    cancelled,
                    ..Default::default()
                })
            }
        }
    }

    pub fn save(
        self,
        loaded: &LoadedConfig,
        target: &SaveTarget,
    ) -> anyhow::Result<Option<PathBuf>> {
        loaded.config.validate()?;
        match self {
            Self::Live => loaded.save(target).map(Some),
            Self::Demo => Ok(None),
        }
    }
}

pub fn synthetic_config() -> Config {
    Config {
        ready_root: PathBuf::from("【演示】/工作待归档素材"),
        archived_root: PathBuf::from("【演示】/已归档永久资料库"),
        system_root: PathBuf::from("【演示】/内存元数据"),
        reserve_gb: 50,
        stable_minutes: 30,
        min_drive_gb: 64,
        name_prefix: "演示盘".into(),
        test_archives: true,
        winrar_path: PathBuf::new(),
        bandizip_path: PathBuf::new(),
        seven_zip_path: PathBuf::new(),
        extra_catalogs: Vec::new(),
        watch_source: None,
        enable_watch: false,
        watch_poll_secs: 60,
        incremental_verify: "on_suspect".into(),
    }
}

pub fn demo_drives() -> Vec<DriveInfo> {
    [
        ("DEMO-E", "演示盘1", 650, 2000, false),
        ("DEMO-F", "演示盘2", 120, 4000, true),
    ]
    .into_iter()
    .map(|(letter, id, free, total, sealed)| DriveInfo {
        letter: letter.into(),
        root: PathBuf::from(format!("【演示】/{letter}")),
        id: id.into(),
        sealed,
        free_bytes: free * 1024 * 1024 * 1024,
        total_bytes: total * 1024 * 1024 * 1024,
    })
    .collect()
}

pub fn demo_plan(cfg: &Config, opts: &Options) -> ArchivePlan {
    let names = [
        "2026秋季高清纪录片合集_主拍摄机位A",
        "bftool桌面应用界面高保真交互规范与重构源码_最终交付版",
        "临时渲染输出缓存_非永久归档",
    ];
    ArchivePlan {
        drive: demo_drives().remove(0),
        opts: opts.clone(),
        items: names
            .into_iter()
            .enumerate()
            .map(|(i, name)| PlanItem {
                name: name.into(),
                source_root: cfg.ready_root.clone(),
                source_path: cfg.ready_root.join(name),
                source_relative: PathBuf::from(name),
                est_bytes: [78, 4, 12][i] * 1024 * 1024 * 1024,
                dest_existed_at_plan: false,
                action: if i < 2 {
                    PlanAction::Archive {
                        dest_name: name.into(),
                    }
                } else {
                    PlanAction::Skip("【演示】临时非归档目录，按名称规则忽略".into())
                },
            })
            .collect(),
    }
}

fn simulate(cancel: &AtomicBool, reporter: &dyn Reporter) -> bool {
    reporter.info("【演示合成任务】纯内存模拟，不读取文件、不操作真实卷");
    let mut progress = reporter.progress_bytes("【演示】内存模拟进度", 60);
    for _ in 0..60 {
        if cancel.load(Ordering::Relaxed) {
            reporter.info("【演示】模拟任务已取消");
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
        progress.inc(1);
    }
    progress.finish();
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use bftool_core::reporter::NoopReporter;
    use bftool_core::service::FileFilter;

    #[test]
    fn demo_every_operation_uses_memory_even_with_unusable_paths() {
        let backend = Backend::Demo;
        let mut loaded = backend.load_config().unwrap();
        assert!(matches!(loaded.source, ConfigSource::Default));
        // Deliberately invalid local names: never an addressable volume/network host.
        loaded.config.ready_root = PathBuf::from("?:/<demo-ready>");
        loaded.config.archived_root = PathBuf::from("?:/<demo-archive>");
        loaded.config.system_root = PathBuf::from("?:/<demo-system>");
        loaded.config.extra_catalogs = vec![PathBuf::from("?:/<demo-index>.csv")];
        let cfg = &loaded.config;
        let reporter = NoopReporter;
        let cancel = AtomicBool::new(true);
        let opts = Options::default();
        assert_eq!(backend.status(cfg).unwrap().pending_count, 3);
        let drives = backend.drives().unwrap();
        assert!(drives.iter().all(|d| d.letter.starts_with("DEMO-")));
        assert_eq!(backend.init_candidates(cfg).unwrap().len(), 3);
        assert!(backend.last_verify(cfg, &drives[0].id).unwrap().is_some());
        assert!(backend.offline_drives(cfg, &drives).is_empty());
        let plan = backend.plan(cfg, &opts, &reporter).unwrap();
        assert_eq!(plan.items.len(), 3);
        assert!(
            backend
                .archive(cfg, &plan, &cancel, &reporter)
                .unwrap()
                .cancelled
        );
        assert!(
            backend
                .verify(cfg, Some("?:/invalid"), None, &cancel, &reporter)
                .unwrap()
                .cancelled
        );
        assert_eq!(
            backend
                .verify(
                    cfg,
                    None,
                    Some(Path::new("?:/<invalid-file>")),
                    &cancel,
                    &reporter
                )
                .unwrap()
                .checked,
            0
        );
        assert_eq!(backend.find(cfg, "纪录片").unwrap().matches.len(), 2);
        backend
            .initialize(cfg, "DEMO-G", Some("demo-only"), false, &reporter)
            .unwrap();
        assert!(backend.watch_folder_exists(Path::new("?:/<watch-folder>")));
        assert!(!backend.watch_folder_exists(Path::new("")));
        let req = WatchRequest {
            folder: PathBuf::from("?:/<watch-folder>"),
            files: FileFilter::default(),
            poll_secs: 1,
            once: true,
            archive: opts,
        };
        assert!(
            backend
                .watch(cfg, &req, &cancel, &reporter)
                .unwrap()
                .cancelled
        );
        assert!(backend
            .save(
                &loaded,
                &SaveTarget::Custom(PathBuf::from("?:/<config>.toml"))
            )
            .unwrap()
            .is_none());
    }
}
