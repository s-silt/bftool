//! 事务标记：把「正在执行：移动源 → 写索引」这段不可见的中间状态外部化到文件。
//!
//! 顺序：写标记 → 移动源 → 写清单/索引 → 删标记。
//! 任意一步崩了，下次启动看到标记会按内容自检：
//! - 源还在「待备份」 → 标记残留，自动清理后正常重做
//! - 源已移 + 索引已写 → 完整归档完成，仅标记残留，自动清理
//! - 源已移 + 索引未写 → 半成品，需要人工核对（不自动清理）

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct PendingTxn {
    pub project_dest_name: String,
    pub project_src_name: String,
    pub drive_id: String,
    pub drive_letter: String,
    pub in_drive_path: String,
    pub src_path: String,
    pub move_to: String,
    pub started_at: String,
}

impl PendingTxn {
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).ok();
        }
        let s = format!(
            "项目     : {}\n源名     : {}\n目标盘   : {} ({}:)\n盘内路径 : {}\n源路径   : {}\n将移至   : {}\n时间     : {}\n状态说明 : 源已通过校验，正在执行「移动源 → 写索引」。若此文件残留，说明该步骤被中断，请核对该项目是否已写入索引。\n",
            self.project_dest_name,
            self.project_src_name,
            self.drive_id,
            self.drive_letter,
            self.in_drive_path,
            self.src_path,
            self.move_to,
            self.started_at,
        );
        fs::write(path, s).with_context(|| format!("写事务标记失败：{}", path.display()))?;
        Ok(())
    }

    pub fn read_text(path: &Path) -> Result<String> {
        fs::read_to_string(path).with_context(|| format!("读事务标记失败：{}", path.display()))
    }

    pub fn clear(path: &Path) -> Result<()> {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("清除事务标记失败：{}", path.display()))?;
        }
        Ok(())
    }
}
