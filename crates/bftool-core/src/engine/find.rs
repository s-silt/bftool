//! 根据关键词在全局索引(`备份索引名单.csv`)里找项目落到了哪块盘。
//! core 提供结构化 `search`(GUI 直接消费),CLI `run` 负责渲染表格。(Spec D §4.1 / L-031)

use anyhow::{Context, Result};
use std::fs;

use crate::config::Config;
use crate::engine::paths;

/// 一条查询命中(结构化,GUI 列表 / CLI 表格各自渲染)。
#[derive(Debug, Clone)]
pub struct FindMatch {
    pub folder: String,
    pub drive_id: String,
    pub archived_time: String,
    pub project_no: String,
    pub in_drive_path: String,
    pub verify: String,
}

/// 在全局索引里按关键词(文件夹名或编号 contains)查项目。无总索引 → 空结果。
pub fn search(cfg: &Config, keyword: &str) -> Result<Vec<FindMatch>> {
    let cat = paths::system_global_catalog(&cfg.system_root);
    if !cat.is_file() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&cat).with_context(|| format!("读总索引失败：{}", cat.display()))?;
    let mut rdr = csv::Reader::from_reader(std::io::Cursor::new(bytes));
    let headers = rdr.headers().cloned().unwrap_or_default();
    let idx = |name: &str| headers.iter().position(|h| h == name);
    let i_folder = idx("文件夹名");
    let i_drive = idx("备份盘名");
    let i_time = idx("备份时间");
    let i_no = idx("编号");
    let i_path = idx("盘内路径");
    let i_check = idx("校验方式");

    let get = |rec: &csv::StringRecord, i: Option<usize>| {
        i.and_then(|c| rec.get(c)).unwrap_or("").to_string()
    };

    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        let folder = get(&rec, i_folder);
        let no = get(&rec, i_no);
        if !folder.contains(keyword) && !no.contains(keyword) {
            continue;
        }
        out.push(FindMatch {
            folder,
            drive_id: get(&rec, i_drive),
            archived_time: get(&rec, i_time),
            project_no: no,
            in_drive_path: get(&rec, i_path),
            verify: get(&rec, i_check),
        });
    }
    Ok(out)
}

pub fn run(cfg: &Config, keyword: &str) -> Result<()> {
    let cat = paths::system_global_catalog(&cfg.system_root);
    if !cat.is_file() {
        println!("总索引还不存在（{}）—— 尚未归档过任何项目。", cat.display());
        return Ok(());
    }
    let matches = search(cfg, keyword)?;
    println!(
        "{:<28}{:<10}{:<20}{:<8}{:<24}校验",
        "文件夹名", "备份盘", "备份时间", "编号", "盘内路径"
    );
    println!("{}", "-".repeat(110));
    for m in &matches {
        println!(
            "{:<28}{:<10}{:<20}{:<8}{:<24}{}",
            m.folder, m.drive_id, m.archived_time, m.project_no, m.in_drive_path, m.verify,
        );
    }
    println!("{}", "-".repeat(110));
    println!("匹配 {} 行。", matches.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cfg_with_catalog(dir: &Path, csv: &str) -> Config {
        let sysroot = dir.join("sys");
        std::fs::create_dir_all(&sysroot).unwrap();
        std::fs::write(paths::system_global_catalog(&sysroot), csv).unwrap();
        Config {
            system_root: sysroot,
            ..Config::default()
        }
    }

    #[test]
    fn search_matches_folder_and_no() {
        let d = tempfile::tempdir().unwrap();
        let csv = "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
                   001proj,备份1,2026-05-29,001,项目\\001proj,SHA256-OK\n\
                   002other,备份1,2026-05-29,002,项目\\002other,SHA256-OK\n";
        let cfg = cfg_with_catalog(d.path(), csv);
        let r = search(&cfg, "proj").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].folder, "001proj");
        assert_eq!(r[0].drive_id, "备份1");
        assert_eq!(r[0].verify, "SHA256-OK");
        assert_eq!(search(&cfg, "002").unwrap().len(), 1, "按编号也能匹配");
        assert!(search(&cfg, "zzz").unwrap().is_empty(), "无匹配 → 空");
    }

    #[test]
    fn search_no_catalog_returns_empty() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            system_root: d.path().join("nope"),
            ..Config::default()
        };
        assert!(search(&cfg, "x").unwrap().is_empty());
    }
}
