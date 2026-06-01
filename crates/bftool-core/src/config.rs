//! 配置加载。
//!
//! 加载顺序：CLI `--config`、当前目录 `bftool.toml`、可执行文件目录、`%APPDATA%\bftool\config.toml`、内置默认。
//! 找不到任何配置文件时使用内置默认（仍可工作，仅路径用占位符提醒用户）。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 待归档（源）目录：每个子文件夹 = 一个项目
    pub ready_root: PathBuf,
    /// 已归档目录：校验通过后源被移动到这里（仍在 SSD，永不删除）
    pub archived_root: PathBuf,
    /// 系统/索引目录：全局索引、日志、序号、需人工处理等
    pub system_root: PathBuf,
    /// 每盘预留余量（GB），低于此判定放不下
    #[serde(default = "default_reserve_gb")]
    pub reserve_gb: u64,
    /// 「最近修改 < N 分钟」视为不稳定，跳过
    #[serde(default = "default_stable_minutes")]
    pub stable_minutes: u64,
    /// 自动认盘的最小容量（GB），防止误抓 U 盘
    #[serde(default = "default_min_drive_gb")]
    pub min_drive_gb: u64,
    /// 备份盘命名前缀：「备份1」「备份2」…
    #[serde(default = "default_name_prefix")]
    pub name_prefix: String,

    // -------- Spec B: 压缩包测试(默认开,新手不用动) --------
    /// 默认开启压缩包内部测试(D1)。用 --no-test-archives 关掉。
    #[serde(default = "default_true")]
    pub test_archives: bool,

    /// WinRAR 路径(优先,返回码最明确)。默认值是 PowerShell 旧版 = Windows 安装位置。
    #[serde(default = "default_winrar")]
    pub winrar_path: PathBuf,

    /// Bandizip 路径(次选,控制台版 bz.exe)。
    #[serde(default = "default_bandizip")]
    pub bandizip_path: PathBuf,

    /// 7-Zip 路径(免费开源,推荐新手装这个)。
    #[serde(default = "default_seven_zip")]
    pub seven_zip_path: PathBuf,

    /// 多机汇总查询：其它电脑拷来的「备份索引名单.csv」路径列表。
    /// `find` 查询时与本机总索引一并检索；只读、不影响归档/盘号/事务。
    #[serde(default)]
    pub extra_catalogs: Vec<PathBuf>,
}

fn default_reserve_gb() -> u64 {
    30
}
fn default_stable_minutes() -> u64 {
    30
}
fn default_min_drive_gb() -> u64 {
    200
}
fn default_name_prefix() -> String {
    "备份".to_string()
}

fn default_true() -> bool {
    true
}

fn default_seven_zip() -> PathBuf {
    PathBuf::from(r"C:\Program Files\7-Zip\7z.exe")
}

fn default_winrar() -> PathBuf {
    PathBuf::from(r"C:\Program Files\WinRAR\WinRAR.exe")
}

fn default_bandizip() -> PathBuf {
    PathBuf::from(r"C:\Program Files\Bandizip\bz.exe")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ready_root: PathBuf::from(r"D:\资料库\待备份"),
            archived_root: PathBuf::from(r"D:\资料库\已备份"),
            system_root: PathBuf::from(r"D:\资料库\备份系统"),
            reserve_gb: default_reserve_gb(),
            stable_minutes: default_stable_minutes(),
            min_drive_gb: default_min_drive_gb(),
            name_prefix: default_name_prefix(),
            // 新加:
            test_archives: default_true(),
            winrar_path: default_winrar(),
            bandizip_path: default_bandizip(),
            seven_zip_path: default_seven_zip(),
            extra_catalogs: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        Ok(Self::load_with_source(explicit)?.config)
    }

    /// 加载并记录来源(GUI 需要知道"从哪读、能写回哪")。(Spec D §4.3)
    pub fn load_with_source(explicit: Option<&Path>) -> Result<LoadedConfig> {
        let (config, source) = if let Some(p) = explicit {
            let c = Self::from_path(p)
                .with_context(|| format!("读取 --config 指定的配置文件失败：{}", p.display()))?;
            (c, ConfigSource::Explicit(p.to_path_buf()))
        } else if let Some(cand) = Self::candidate_paths().into_iter().find(|c| c.is_file()) {
            let c = Self::from_path(&cand)
                .with_context(|| format!("读取配置文件失败：{}", cand.display()))?;
            (c, ConfigSource::Candidate(cand))
        } else {
            // 找不到任何配置文件 → 用默认值；用户首次跑 `bftool` 会看到提示
            (Self::default(), ConfigSource::Default)
        };
        match &source {
            ConfigSource::Explicit(p) | ConfigSource::Candidate(p) => config
                .validate()
                .with_context(|| format!("配置文件校验失败：{}", p.display()))?,
            ConfigSource::Default => config.validate().context("内置默认配置校验失败")?,
        }
        Ok(LoadedConfig { config, source })
    }

    /// 加载后校验配置的内在不变量(不依赖具体备份盘;盘相关的根目录关系检查在 safety::check_paths)。
    /// 此前 Config 反序列化后直接到处传,verify/find/drives 等都在用未校验配置。(ledger L-025)
    pub fn validate(&self) -> Result<()> {
        let p = self.name_prefix.trim();
        if p.is_empty() {
            anyhow::bail!("配置 name_prefix 不能为空(用于「备份N」编号识别);建议设为「备份」。");
        }
        if p.chars().all(|c| c.is_ascii_digit()) {
            anyhow::bail!(
                "配置 name_prefix 不能是纯数字「{}」(会让备份盘编号无法解析、序号失效);\
                 建议带非数字前缀,如「备份」。",
                p
            );
        }
        // 三个根目录不能为空/纯空白(详见 check_roots_nonempty)。
        self.check_roots_nonempty()?;
        // extra_catalogs:GUI parse_form 已过滤空白项,但 CLI 直接编辑 toml 可能塞进空串/纯空白。
        // 空路径会被 find 当成"不存在的来源"静默记为失败,徒增噪音;直接在加载期拒掉。(F4)
        for (i, c) in self.extra_catalogs.iter().enumerate() {
            if c.as_os_str().to_string_lossy().trim().is_empty() {
                anyhow::bail!(
                    "配置 extra_catalogs 第 {} 项为空(或纯空白);请删除该项,或填一个有效的「备份索引名单.csv」路径。",
                    i + 1
                );
            }
        }
        Ok(())
    }

    fn candidate_paths() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            out.push(cwd.join("bftool.toml"));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                out.push(dir.join("bftool.toml"));
            }
        }
        if let Some(appdata) = std::env::var_os("APPDATA") {
            out.push(PathBuf::from(appdata).join("bftool").join("config.toml"));
        }
        out
    }

    fn from_path(p: &Path) -> Result<Self> {
        let text = fs::read_to_string(p)?;
        let mut cfg: Self = toml::from_str(&text).context("配置文件 TOML 解析失败")?;
        // 必须在 resolve_root **之前**拒掉空/纯空白根目录:否则 resolve_root 会把空路径
        // 悄悄变成配置文件所在目录(base.join("")),用户得到一个意外的源/索引位置,而
        // 解析后的 validate 看到的已是非空的 base 路径、检查不到。(review-r2 R2-5)
        cfg.check_roots_nonempty()?;
        let base = config_base_dir(p);
        cfg.ready_root = resolve_root(&base, cfg.ready_root);
        cfg.archived_root = resolve_root(&base, cfg.archived_root);
        cfg.system_root = resolve_root(&base, cfg.system_root);
        Ok(cfg)
    }

    /// 三个根目录不能为空/纯空白。空 root 会让 resolve_root(base.join(""))退化成配置目录、
    /// system_root="" 让全局索引落到意外位置。GUI parse_form 已校验,但 CLI 直接编辑 toml 可绕过。(review-r2 R2-5)
    fn check_roots_nonempty(&self) -> Result<()> {
        for (name, p) in [
            ("ready_root（待备份）", &self.ready_root),
            ("archived_root（已备份）", &self.archived_root),
            ("system_root（备份系统）", &self.system_root),
        ] {
            if p.as_os_str().to_string_lossy().trim().is_empty() {
                anyhow::bail!("配置 {} 不能为空/纯空白;请填一个有效的目录路径。", name);
            }
        }
        Ok(())
    }
}

fn config_base_dir(p: &Path) -> PathBuf {
    let parent = p
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(parent))
            .unwrap_or_else(|_| parent.to_path_buf())
    }
}

fn resolve_root(base: &Path, p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

/// 配置来源(GUI 据此知道 CLI 下次会从哪读 → 写回同处不错位)。(Spec D §4.3)
#[derive(Debug, Clone)]
pub enum ConfigSource {
    Explicit(PathBuf),
    Candidate(PathBuf),
    Default,
}

/// 加载结果 = 配置 + 来源。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub source: ConfigSource,
}

/// GUI 设置页保存目标。默认推荐 `AppData`(与 cwd 无关,CLI 与 GUI 双击都查到)。
#[derive(Debug, Clone)]
pub enum SaveTarget {
    /// 写回当前来源(source 为 Default 时报错,让用户选位置)
    CurrentSource,
    /// `%APPDATA%\bftool\config.toml`
    AppData,
    /// 当前工作目录 `bftool.toml`(注意:GUI 双击的 cwd 可能与 CLI 终端运行不同)
    CurrentDir,
    Custom(PathBuf),
}

/// `%APPDATA%\bftool\config.toml`(与 cwd 无关)。
pub fn appdata_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("bftool").join("config.toml"))
}

impl LoadedConfig {
    /// 校验后把配置写到目标,返回实际写入路径。(Spec D §4.3)
    pub fn save(&self, target: &SaveTarget) -> Result<PathBuf> {
        self.config.validate()?;
        let path = match target {
            SaveTarget::CurrentSource => match &self.source {
                ConfigSource::Explicit(p) | ConfigSource::Candidate(p) => p.clone(),
                ConfigSource::Default => {
                    anyhow::bail!("当前没有配置文件来源,请选择保存位置(推荐 %APPDATA%)。")
                }
            },
            SaveTarget::AppData => {
                appdata_config_path().context("无法定位 %APPDATA%,请改存到其它位置。")?
            }
            SaveTarget::CurrentDir => std::env::current_dir()
                .context("无法获取当前目录")?
                .join("bftool.toml"),
            SaveTarget::Custom(p) => p.clone(),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
        }
        let body = toml::to_string_pretty(&self.config).context("序列化配置失败")?;
        crate::engine::durable::write_synced(&path, body.as_bytes())
            .with_context(|| format!("写配置失败：{}", path.display()))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── L-025: Config::validate 守住内在不变量 ──
    #[test]
    fn validate_accepts_default() {
        assert!(Config::default().validate().is_ok());
    }

    // ── review-r2 R2-5:三根目录不能为空/纯空白(CLI/TOML 绕过 GUI 校验)──
    #[test]
    fn validate_rejects_empty_or_blank_root() {
        let empty_ready = Config {
            ready_root: PathBuf::from(""),
            ..Config::default()
        };
        assert!(empty_ready.validate().is_err(), "空 ready_root 应被拒");
        let blank_system = Config {
            system_root: PathBuf::from("   "),
            ..Config::default()
        };
        assert!(
            blank_system.validate().is_err(),
            "纯空白 system_root 应被拒"
        );
        let empty_archived = Config {
            archived_root: PathBuf::from(""),
            ..Config::default()
        };
        assert!(
            empty_archived.validate().is_err(),
            "空 archived_root 应被拒"
        );
    }

    // ── review-r2 R2-5:TOML 里空根目录必须在 resolve_root **之前**被拒,
    // 否则会被 base.join("") 悄悄解析成配置文件所在目录而绕过 validate ──
    #[test]
    fn from_path_rejects_empty_root_before_resolve() {
        let d = tempfile::tempdir().unwrap();
        let toml_path = d.path().join("bftool.toml");
        std::fs::write(
            &toml_path,
            "ready_root = \"\"\narchived_root = \"D:/a\"\nsystem_root = \"D:/s\"\n",
        )
        .unwrap();
        assert!(
            Config::load(Some(&toml_path)).is_err(),
            "TOML 空 ready_root 应在加载期被拒(不得解析成配置目录)"
        );
    }

    // ── Spec D §4.3: LoadedConfig::save ──
    #[test]
    fn save_custom_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("bftool.toml");
        let lc = LoadedConfig {
            config: Config::default(),
            source: ConfigSource::Default,
        };
        let written = lc.save(&SaveTarget::Custom(p.clone())).unwrap();
        assert_eq!(written, p);
        let back = Config::from_path(&p).unwrap();
        assert_eq!(back.ready_root, Config::default().ready_root);
        assert_eq!(back.name_prefix, Config::default().name_prefix);
    }

    #[test]
    fn save_current_source_errs_when_default() {
        let lc = LoadedConfig {
            config: Config::default(),
            source: ConfigSource::Default,
        };
        assert!(lc.save(&SaveTarget::CurrentSource).is_err());
    }

    #[test]
    fn validate_rejects_empty_prefix() {
        let c = Config {
            name_prefix: String::new(),
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_all_digit_prefix() {
        let c = Config {
            name_prefix: "123".into(),
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }

    // ── F4: extra_catalogs 不接受空串/纯空白项(CLI 直接编辑 toml 可能塞空串)──
    #[test]
    fn validate_rejects_empty_extra_catalog_entry() {
        let c = Config {
            extra_catalogs: vec![PathBuf::from("")],
            ..Config::default()
        };
        assert!(c.validate().is_err(), "空串路径应被拒绝");

        let c = Config {
            extra_catalogs: vec![PathBuf::from("   ")],
            ..Config::default()
        };
        assert!(c.validate().is_err(), "纯空白路径应被拒绝");
    }

    #[test]
    fn validate_accepts_nonempty_extra_catalogs() {
        let c = Config {
            extra_catalogs: vec![PathBuf::from("D:/汇总/A.csv")],
            ..Config::default()
        };
        assert!(c.validate().is_ok());
    }

    // ── 向后兼容：旧配置文件没有 extra_catalogs 字段，仍应能加载（默认空）──
    #[test]
    fn loads_legacy_config_without_extra_catalogs() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bftool.toml");
        std::fs::write(
            &p,
            "ready_root = \"D:/r\"\n\
             archived_root = \"D:/a\"\n\
             system_root = \"D:/s\"\n",
        )
        .unwrap();
        let c = Config::from_path(&p).unwrap();
        assert!(c.extra_catalogs.is_empty());
        assert_eq!(c.name_prefix, "备份");
    }

    #[test]
    fn rejects_unknown_config_field() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bftool.toml");
        std::fs::write(
            &p,
            "ready_root = \"D:/r\"\n\
             archived_root = \"D:/a\"\n\
             system_root = \"D:/s\"\n\
             reserve_gbb = 500\n",
        )
        .unwrap();

        let err = Config::from_path(&p).unwrap_err();

        assert!(
            format!("{:#}", err).contains("reserve_gbb"),
            "unknown field should be named in error: {err:#}"
        );
    }

    #[test]
    fn from_path_resolves_relative_roots_against_config_file() {
        let d = tempfile::tempdir().unwrap();
        let cfg_dir = d.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let p = cfg_dir.join("bftool.toml");
        std::fs::write(
            &p,
            "ready_root = \"library/ready\"\n\
             archived_root = \"library/archived\"\n\
             system_root = \"library/system\"\n",
        )
        .unwrap();

        let c = Config::from_path(&p).unwrap();

        assert_eq!(c.ready_root, cfg_dir.join("library/ready"));
        assert_eq!(c.archived_root, cfg_dir.join("library/archived"));
        assert_eq!(c.system_root, cfg_dir.join("library/system"));
    }

    #[test]
    fn load_explicit_validation_error_mentions_config_path() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bad.toml");
        std::fs::write(
            &p,
            "ready_root = \"D:/r\"\n\
             archived_root = \"D:/a\"\n\
             system_root = \"D:/s\"\n\
             extra_catalogs = [\"\"]\n",
        )
        .unwrap();

        let err = Config::load_with_source(Some(&p)).unwrap_err();
        let text = format!("{:#}", err);

        assert!(text.contains("配置文件校验失败"));
        assert!(text.contains("bad.toml"));
    }

    #[test]
    fn extra_catalogs_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bftool.toml");
        let lc = LoadedConfig {
            config: Config {
                extra_catalogs: vec![PathBuf::from("D:/u/A.csv"), PathBuf::from("D:/u/B.csv")],
                ..Config::default()
            },
            source: ConfigSource::Default,
        };
        lc.save(&SaveTarget::Custom(p.clone())).unwrap();
        let back = Config::from_path(&p).unwrap();
        assert_eq!(back.extra_catalogs.len(), 2);
        assert_eq!(back.extra_catalogs[0], PathBuf::from("D:/u/A.csv"));
    }
}
