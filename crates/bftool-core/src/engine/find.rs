//! 根据关键词在全局索引(`备份索引名单.csv`)里找项目落到了哪块盘。
//! 支持多机汇总：本机总索引 + 配置里 `extra_catalogs` 的其它电脑索引一并检索。
//! core 提供结构化 `search`(GUI 直接消费),CLI `run` 负责渲染表格。(Spec D §4.1 / L-031)

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

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
    /// 该行来自哪个索引来源("本机" 或额外索引文件的名字)，多机汇总查询时区分用。
    pub source: String,
}

/// 一次查询的结果:命中行 + 检索了几个来源 + 哪些额外来源读不了(供 UI 提示)。
#[derive(Debug, Clone, Default)]
pub struct FindOutcome {
    pub matches: Vec<FindMatch>,
    /// 实际成功检索到的索引来源数(本机 + 可读的额外来源)。
    pub sources_searched: usize,
    /// 读取失败的额外来源标签(文件不存在/解析失败);本机索引不存在不算失败(=尚未归档)。
    pub sources_failed: Vec<String>,
}

/// 额外索引来源的显示标签:取文件名(不含扩展名);拿不到则退回完整路径。
/// 用户按指引把各机文件重命名为 A.csv/B.csv 时，标签就是 A/B，一眼区分来源。
fn catalog_label(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// 读单个索引 CSV，把命中关键词的行追加进 `out`(按 文件夹/盘/编号 去重)。
/// 与历史实现一致:按表头名动态找列、逐行 `contains`、坏行自动跳过。
fn read_catalog(
    path: &Path,
    source: &str,
    keyword: &str,
    out: &mut Vec<FindMatch>,
    seen: &mut HashSet<(String, String, String)>,
) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("读索引失败：{}", path.display()))?;
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

    for rec in rdr.records().flatten() {
        let folder = get(&rec, i_folder);
        let no = get(&rec, i_no);
        if !folder.contains(keyword) && !no.contains(keyword) {
            continue;
        }
        let drive_id = get(&rec, i_drive);
        // 跨来源去重:同一项目(文件夹+盘+编号)只留一条,避免误把本机也加成额外来源时出现重复。
        if !seen.insert((folder.clone(), drive_id.clone(), no.clone())) {
            continue;
        }
        out.push(FindMatch {
            folder,
            drive_id,
            archived_time: get(&rec, i_time),
            project_no: no,
            in_drive_path: get(&rec, i_path),
            verify: get(&rec, i_check),
            source: source.to_string(),
        });
    }
    Ok(())
}

/// 在「本机总索引 + 配置的额外索引」里按关键词(文件夹名或编号 contains)查项目。
pub fn search(cfg: &Config, keyword: &str) -> Result<FindOutcome> {
    let mut out = FindOutcome::default();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();

    // 本机总索引:不存在 = 尚未归档,正常,不计入失败;能读但损坏则向上报错(与历史一致)。
    let main = paths::system_global_catalog(&cfg.system_root);
    if main.is_file() {
        read_catalog(&main, "本机", keyword, &mut out.matches, &mut seen)?;
        out.sources_searched += 1;
    }

    // 额外来源:读不了(文件没了/解析失败)只记失败、跳过,绝不让整次查询失败。
    for p in &cfg.extra_catalogs {
        let label = catalog_label(p);
        if !p.is_file() {
            out.sources_failed.push(label);
            continue;
        }
        match read_catalog(p, &label, keyword, &mut out.matches, &mut seen) {
            Ok(()) => out.sources_searched += 1,
            Err(_) => out.sources_failed.push(label),
        }
    }

    Ok(out)
}

pub fn run(cfg: &Config, keyword: &str) -> Result<()> {
    let outcome = search(cfg, keyword)?;
    if outcome.sources_searched == 0 {
        if outcome.sources_failed.is_empty() {
            println!("还没有任何可检索的索引 —— 尚未归档过任何项目，也没有配置额外索引来源。");
        } else {
            println!(
                "没有可检索的索引：本机尚未归档，且 {} 个额外索引来源都读取失败：{}",
                outcome.sources_failed.len(),
                outcome.sources_failed.join("、")
            );
        }
        return Ok(());
    }
    println!(
        "{:<28}{:<10}{:<10}{:<20}{:<8}{:<24}校验",
        "文件夹名", "来源", "备份盘", "备份时间", "编号", "盘内路径"
    );
    println!("{}", "-".repeat(120));
    for m in &outcome.matches {
        println!(
            "{:<28}{:<10}{:<10}{:<20}{:<8}{:<24}{}",
            m.folder,
            m.source,
            m.drive_id,
            m.archived_time,
            m.project_no,
            m.in_drive_path,
            m.verify,
        );
    }
    println!("{}", "-".repeat(120));
    println!(
        "匹配 {} 行(检索了 {} 个索引来源)。",
        outcome.matches.len(),
        outcome.sources_searched
    );
    if !outcome.sources_failed.is_empty() {
        println!(
            "注意：{} 个额外索引来源读取失败，已跳过：{}",
            outcome.sources_failed.len(),
            outcome.sources_failed.join("、")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

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
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].folder, "001proj");
        assert_eq!(r.matches[0].drive_id, "备份1");
        assert_eq!(r.matches[0].verify, "SHA256-OK");
        assert_eq!(r.matches[0].source, "本机");
        assert_eq!(r.sources_searched, 1);
        assert!(r.sources_failed.is_empty());
        assert_eq!(
            search(&cfg, "002").unwrap().matches.len(),
            1,
            "按编号也能匹配"
        );
        assert!(
            search(&cfg, "zzz").unwrap().matches.is_empty(),
            "无匹配 → 空"
        );
    }

    #[test]
    fn search_no_catalog_returns_empty() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            system_root: d.path().join("nope"),
            ..Config::default()
        };
        let r = search(&cfg, "x").unwrap();
        assert!(r.matches.is_empty());
        assert_eq!(r.sources_searched, 0, "本机索引不存在 → 0 个来源");
        assert!(r.sources_failed.is_empty(), "本机索引不存在不算失败");
    }

    #[test]
    fn search_merges_extra_catalogs() {
        let d = tempfile::tempdir().unwrap();
        // 本机:备份1 上有一个 proj
        let cfg_base = cfg_with_catalog(
            d.path(),
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             婚礼proj,A备份2,2026-05-29,007,项目\\007,SHA256-OK\n",
        );
        // 额外来源 B.csv:C备份1 上另有一个 proj
        let bcsv = d.path().join("B.csv");
        std::fs::write(
            &bcsv,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             婚礼old,C备份1,2025-09-15,003,项目\\003,SHA256-OK\n",
        )
        .unwrap();
        let cfg = Config {
            extra_catalogs: vec![bcsv],
            ..cfg_base
        };
        let r = search(&cfg, "婚礼").unwrap();
        assert_eq!(r.matches.len(), 2, "本机 + 额外来源都命中");
        assert_eq!(r.sources_searched, 2);
        // 额外来源的 source 标签 = 文件名 stem "B"
        assert!(r
            .matches
            .iter()
            .any(|m| m.source == "B" && m.drive_id == "C备份1"));
        assert!(r
            .matches
            .iter()
            .any(|m| m.source == "本机" && m.drive_id == "A备份2"));
    }

    #[test]
    fn search_skips_missing_extra_source() {
        let d = tempfile::tempdir().unwrap();
        let cfg_base = cfg_with_catalog(
            d.path(),
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             proj,备份1,2026-05-29,001,项目\\001,SHA256-OK\n",
        );
        let cfg = Config {
            extra_catalogs: vec![PathBuf::from("Z:/不存在/没有.csv")],
            ..cfg_base
        };
        let r = search(&cfg, "proj").unwrap();
        assert_eq!(r.matches.len(), 1, "本机仍能查到");
        assert_eq!(r.sources_searched, 1);
        assert_eq!(r.sources_failed.len(), 1, "缺失的额外来源记为失败");
        assert_eq!(r.sources_failed[0], "没有");
    }

    #[test]
    fn search_dedups_same_project_across_sources() {
        let d = tempfile::tempdir().unwrap();
        let row = "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
                   proj,备份1,2026-05-29,001,项目\\001,SHA256-OK\n";
        let cfg_base = cfg_with_catalog(d.path(), row);
        // 把本机索引文件原样也当额外来源加进来 → 应去重,不出现两行
        let dup = paths::system_global_catalog(&cfg_base.system_root);
        let cfg = Config {
            extra_catalogs: vec![dup],
            ..cfg_base
        };
        let r = search(&cfg, "proj").unwrap();
        assert_eq!(r.matches.len(), 1, "同一项目跨来源去重");
    }
}
