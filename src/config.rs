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
}

fn default_reserve_gb() -> u64 { 30 }
fn default_stable_minutes() -> u64 { 30 }
fn default_min_drive_gb() -> u64 { 200 }
fn default_name_prefix() -> String { "备份".to_string() }

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
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        if let Some(p) = explicit {
            return Self::from_path(p).with_context(|| format!("读取 --config 指定的配置文件失败：{}", p.display()));
        }

        for candidate in Self::candidate_paths() {
            if candidate.is_file() {
                return Self::from_path(&candidate)
                    .with_context(|| format!("读取配置文件失败：{}", candidate.display()));
            }
        }

        // 找不到任何配置文件 → 用默认值；用户首次跑 `bftool` 会看到提示
        Ok(Self::default())
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
