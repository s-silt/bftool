//! 复查记录:把每块盘最近一次复查的结果(时间 + 结论)记到本地 `system_root`,
//! 让仪表盘能显示"上次复查 + 当次是否发现损坏"。
//!
//! **verify 对备份盘严格只读**,故复查记录写在本机 system_root(`复查记录.csv`),不碰盘 ——
//! 这样封盘/写保护盘也能复查,且不混淆"元数据写失败"与"数据损坏"。(Spec D §4.4)

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::path::Path;

use crate::engine::paths;
use crate::engine::verify::VerifyOutcome;

/// 某盘最近一次复查记录。
#[derive(Debug, Clone)]
pub struct LastVerify {
    pub drive_id: String,
    pub when: String, // RFC3339 UTC
    pub status: VerifyOutcome,
}

/// 记录/更新某盘最近复查结果(同 drive_id 去重保最新)。**Cancelled 不记**(没跑完)。
pub fn record_verify(system_root: &Path, drive_id: &str, outcome: &VerifyOutcome) -> Result<()> {
    if matches!(outcome, VerifyOutcome::Cancelled) {
        return Ok(());
    }
    let path = paths::system_verify_log(system_root);
    let mut rows =
        read_rows(&path).with_context(|| format!("读取复查记录失败：{}", path.display()))?;
    rows.retain(|r| r.0 != drive_id);
    let (status, bad, extra) = encode(outcome);
    rows.push((
        drive_id.to_string(),
        Utc::now().to_rfc3339(),
        status,
        bad,
        extra,
    ));
    write_rows(&path, &rows)
}

/// 读某盘最近复查记录(无记录 → None)。
pub fn read_last_verify(system_root: &Path, drive_id: &str) -> Result<Option<LastVerify>> {
    let path = paths::system_verify_log(system_root);
    let rows = read_rows(&path).with_context(|| format!("读取复查记录失败：{}", path.display()))?;
    Ok(rows
        .into_iter()
        .find(|r| r.0 == drive_id)
        .map(|(id, when, status, bad, extra)| LastVerify {
            drive_id: id,
            when,
            status: decode(&status, bad, extra),
        }))
}

fn encode(o: &VerifyOutcome) -> (String, u64, u64) {
    match o {
        VerifyOutcome::Clean => ("Clean".into(), 0, 0),
        VerifyOutcome::IssuesFound { bad } => ("IssuesFound".into(), *bad, 0),
        VerifyOutcome::ExtraOnly { extra } => ("ExtraOnly".into(), 0, *extra),
        VerifyOutcome::Cancelled => ("Cancelled".into(), 0, 0),
    }
}

fn decode(status: &str, bad: u64, extra: u64) -> VerifyOutcome {
    match status {
        "IssuesFound" => VerifyOutcome::IssuesFound { bad },
        "ExtraOnly" => VerifyOutcome::ExtraOnly { extra },
        "Clean" => VerifyOutcome::Clean,
        // Cancelled 表示"跑完但未记录确认的结果",比静默 Clean 安全:
        // 未知/损坏的 status 字段不应静默显示为"无问题"。
        _ => VerifyOutcome::Cancelled,
    }
}

type Row = (String, String, String, u64, u64);

fn read_rows(path: &Path) -> Result<Vec<Row>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let mut rdr = csv::Reader::from_path(path)?;
    let mut out = Vec::new();
    for (idx, rec) in rdr.records().enumerate() {
        let rec = rec.with_context(|| format!("复查记录第 {} 行格式错误", idx + 2))?;
        let id = rec.get(0).unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let when = rec.get(1).unwrap_or("").to_string();
        let status = rec.get(2).unwrap_or("").to_string();
        let bad = parse_count(rec.get(3), "bad", idx + 2)?;
        let extra = parse_count(rec.get(4), "extra", idx + 2)?;
        out.push((id, when, status, bad, extra));
    }
    Ok(out)
}

fn parse_count(raw: Option<&str>, field: &str, line: usize) -> Result<u64> {
    let Some(raw) = raw else { return Ok(0) };
    if raw.trim().is_empty() {
        return Ok(0);
    }
    raw.parse::<u64>()
        .with_context(|| format!("复查记录第 {} 行 {} 不是数字：{}", line, field, raw))
}

fn write_rows(path: &Path, rows: &[Row]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建复查记录目录失败：{}", parent.display()))?;
    }
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record(["drive_id", "when", "status", "bad", "extra"])?;
    for r in rows {
        wtr.write_record([
            r.0.as_str(),
            r.1.as_str(),
            r.2.as_str(),
            &r.3.to_string(),
            &r.4.to_string(),
        ])?;
    }
    let bytes = wtr
        .into_inner()
        .map_err(|e| anyhow!("刷新复查记录缓冲失败：{}", e))?;
    crate::engine::durable::write_synced(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_read_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let sr = d.path();
        assert!(
            read_last_verify(sr, "备份1").unwrap().is_none(),
            "无记录 → None"
        );

        record_verify(sr, "备份1", &VerifyOutcome::IssuesFound { bad: 2 }).unwrap();
        record_verify(sr, "备份2", &VerifyOutcome::Clean).unwrap();
        let lv = read_last_verify(sr, "备份1").unwrap().unwrap();
        assert_eq!(lv.drive_id, "备份1");
        assert_eq!(lv.status, VerifyOutcome::IssuesFound { bad: 2 });
        assert!(!lv.when.is_empty());

        // 同盘更新 → 保最新
        record_verify(sr, "备份1", &VerifyOutcome::Clean).unwrap();
        assert_eq!(
            read_last_verify(sr, "备份1").unwrap().unwrap().status,
            VerifyOutcome::Clean
        );

        // Cancelled 不记
        record_verify(sr, "备份3", &VerifyOutcome::Cancelled).unwrap();
        assert!(read_last_verify(sr, "备份3").unwrap().is_none());
    }

    #[test]
    fn record_verify_rejects_malformed_existing_log() {
        let d = tempfile::tempdir().unwrap();
        let sr = d.path();
        std::fs::create_dir_all(sr).unwrap();
        std::fs::write(
            paths::system_verify_log(sr),
            "drive_id,when,status,bad,extra\n\"unterminated",
        )
        .unwrap();

        let err = record_verify(sr, "备份1", &VerifyOutcome::Clean).unwrap_err();
        assert!(
            err.to_string().contains("读取复查记录失败"),
            "error should mention verify log read failure: {err}"
        );
        assert!(
            read_last_verify(sr, "备份1").is_err(),
            "corrupt verify log must not be displayed as missing history"
        );
    }
}
