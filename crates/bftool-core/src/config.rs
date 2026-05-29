//! 配置加载。
//!
//! 加载顺序：CLI `--config`、当前目录 `bftool.toml`、可执行文件目录、`%APPDATA%\bftool\config.toml`、内置默认。
//! 找不到任何配置文件时使用内置默认（仍可工作，仅路径用占位符提醒用户）。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let cfg = if let Some(p) = explicit {
            Self::from_path(p)
                .with_context(|| format!("读取 --config 指定的配置文件失败：{}", p.display()))?
        } else if let Some(c) = Self::candidate_paths().iter().find(|c| c.is_file()) {
            Self::from_path(c).with_context(|| format!("读取配置文件失败：{}", c.display()))?
        } else {
            // 找不到任何配置文件 → 用默认值；用户首次跑 `bftool` 会看到提示
            Self::default()
        };
        cfg.validate()?;
        Ok(cfg)
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
        let cfg: Self = toml::from_str(&text).context("配置文件 TOML 解析失败")?;
        Ok(cfg)
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
}
