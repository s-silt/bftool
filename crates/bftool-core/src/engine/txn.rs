//! 事务标记：把「已校验通过、正在执行 写索引 → 移动源」这段不可见的中间状态外部化到文件。
//!
//! 顺序：写标记 → 写清单/索引 → 移动源 → 删标记（索引先于移源落盘，任一步失败源仍可重做）。
//! 任意一步崩了，下次启动看到标记会按内容自检（见 `archive::check_pending_txn`）：
//! - 源还在「待备份」 → 移源尚未发生（索引可能已写）→ 自动重做（`catalog_has_project` 幂等，
//!   重做改用带时间戳的唯一名，不覆盖已写的副本/索引行）
//! - 源已移 + 索引已写 → 完整归档完成，仅标记残留，自动清理
//! - 源已移 + 索引未写 → 罕见半成品（移源成功但索引漏写），需要人工核对（不自动清理）

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
        // 结构化(TOML)写入 + 人类可读注释头;读回用 PendingTxn::read 反序列化,
        // 不再靠中文标签字符串抓取(grab_field),避免改标签 → 恢复解析静默失效。(ledger L-022)
        let body = toml::to_string_pretty(self).context("序列化事务标记失败")?;
        let s = format!(
            "# bftool 进行中事务标记\n\
             # 源已通过校验,正在执行「写索引 → 移动源」。若此文件残留,说明该步骤被中断。\n\
             # 多数情况下源仍在「待备份」(索引或已写),bftool 下次启动会自动安全重做;\n\
             # 仅在罕见的「已移源但索引漏写」情形下会提示人工核对。\n\
             {body}"
        );
        // 用 fsync 写入:事务标记必须先于"移源"真正落盘,否则断电后标记丢失、
        // 源却已 rename → check_pending_txn 看不到标记 → 项目静默消失。(ledger L-001)
        crate::engine::durable::write_synced(path, s.as_bytes())
            .with_context(|| format!("写事务标记失败：{}", path.display()))?;
        Ok(())
    }

    /// 读回并反序列化事务标记(TOML;文件里的 `#` 注释行会被忽略)。(ledger L-022)
    pub fn read(path: &Path) -> Result<PendingTxn> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("读事务标记失败：{}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("解析事务标记失败：{}", path.display()))
    }

    pub fn clear(path: &Path) -> Result<()> {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("清除事务标记失败：{}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── L-022: write → read 八字段无损往返(替代脆弱的 grab_field 标签抓取) ──
    #[test]
    fn pending_txn_round_trips_all_fields() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("进行中事务.txt");
        let txn = PendingTxn {
            project_dest_name: "001proj_20260529".into(),
            project_src_name: "001proj".into(),
            drive_id: "备份1".into(),
            drive_letter: "E".into(),
            in_drive_path: "项目\\001proj_20260529".into(),
            src_path: "D:\\资料库\\待备份\\001proj".into(),
            move_to: "D:\\资料库\\已备份\\001proj".into(),
            started_at: "2026-05-29 12:00:00".into(),
        };
        txn.write(&p).unwrap();
        let back = PendingTxn::read(&p).unwrap();
        assert_eq!(back.project_dest_name, txn.project_dest_name);
        assert_eq!(back.project_src_name, txn.project_src_name);
        assert_eq!(back.drive_id, txn.drive_id);
        assert_eq!(back.drive_letter, txn.drive_letter);
        assert_eq!(back.in_drive_path, txn.in_drive_path);
        assert_eq!(back.src_path, txn.src_path); // 反斜杠路径经 TOML 转义往返无损
        assert_eq!(back.move_to, txn.move_to);
        assert_eq!(back.started_at, txn.started_at);
    }

    #[test]
    fn clear_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("进行中事务.txt");
        PendingTxn::clear(&p).unwrap(); // 不存在也 Ok
        fs::write(&p, "x").unwrap();
        PendingTxn::clear(&p).unwrap();
        assert!(!p.exists());
    }
}
