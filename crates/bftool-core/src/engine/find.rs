//! 根据关键词在全局索引(`备份索引名单.csv`)里找项目落到了哪块盘。
//! 支持多机汇总：本机总索引 + 配置里 `extra_catalogs` 的其它电脑索引一并检索。
//! core 提供结构化 `search`(GUI 直接消费),CLI `run` 负责渲染表格。(Spec D §4.1 / L-031)

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::engine::paths;

/// 本机总索引来源在结果里的来源标签。源码与测试统一引用,避免中文字面量重复。(F5)
pub const LOCAL_SOURCE: &str = "本机";

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
    /// 读取失败的来源标签(额外来源不存在、或任何来源读取/解析失败)；本机索引**不存在**不算失败(=尚未归档)，但本机索引存在却读取失败会计入此列表。
    pub sources_failed: Vec<String>,
}

/// 整行去重 key(全部展示字段)。只折叠"逐字段完全相同"的行 —— 即同一份索引被
/// 重复导入的情况;跨机器的不同项目只要有任一字段不同(日期/盘内路径/校验…)就都保留,
/// 绝不静默吞掉。盘号默认每机各自从 1 编(`备份1`…)、编号取文件夹前导数字,二者都
/// 不是跨机唯一,所以**不能**只用 (文件夹,盘,编号) 当 key。(review #1/#3)
type RowKey = (String, String, String, String, String, String);

/// 默认索引文件名去扩展名后的 stem(用户不改名时,多份都叫这个)。
///
/// 运行时从 `GLOBAL_CATALOG_FILE`(目前 "备份索引名单.csv")解析 stem 而非编译期常量,
/// 是为了跟随 paths 模块那个唯一来源、避免两处字面量漂移。代价仅是每次查询额外几次廉价
/// 路径解析(find 不在热路径)。`unwrap_or("备份索引名单")` 是 fallback:仅当 `GLOBAL_CATALOG_FILE`
/// 被改成无 stem 的形态(如纯扩展名 ".csv")时才会触发,正常文件名不会走到。(F6)
fn default_catalog_stem() -> &'static str {
    Path::new(paths::GLOBAL_CATALOG_FILE)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("备份索引名单")
}

/// 额外索引来源的显示标签:优先用文件名(不含扩展名);若就是默认文件名(用户没改名),
/// 改用所在文件夹名,免得多份来源都显示成同一个"备份索引名单"而无法区分。(review #2)
fn catalog_label(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str());
    if stem == Some(default_catalog_stem()) {
        if let Some(parent) = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
        {
            if !parent.is_empty() {
                return parent.to_string();
            }
        }
    }
    stem.map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

/// 读单个索引 CSV，把命中关键词的行追加进 `out`(按整行去重)。
/// 与历史实现一致:按表头名动态找列、逐行 `contains`、坏行自动跳过。
/// 若「文件夹名」「编号」两个关键列都不存在 → 判定为无法识别的索引格式并返回错误,
/// 交调用方记为"读取失败";否则选错文件会悄悄返回 0 条、却让人误以为查过了。(review #6)
fn read_catalog(
    path: &Path,
    source: &str,
    keyword: &str,
    out: &mut Vec<FindMatch>,
    seen: &mut HashSet<RowKey>,
) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("读索引失败：{}", path.display()))?;
    let mut rdr = csv::Reader::from_reader(std::io::Cursor::new(bytes));
    let headers = rdr.headers().cloned().unwrap_or_default();
    let idx = |name: &str| headers.iter().position(|h| h == name);
    let i_folder = idx("文件夹名");
    let i_no = idx("编号");
    if i_folder.is_none() && i_no.is_none() {
        let header_list: Vec<&str> = headers.iter().collect();
        anyhow::bail!(
            "无法识别的索引格式(缺「文件夹名」「编号」列)：{}，实际表头：[{}]",
            path.display(),
            header_list.join(", ")
        );
    }
    let i_drive = idx("备份盘名");
    let i_time = idx("备份时间");
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
        let m = FindMatch {
            folder,
            drive_id: get(&rec, i_drive),
            archived_time: get(&rec, i_time),
            project_no: no,
            in_drive_path: get(&rec, i_path),
            verify: get(&rec, i_check),
            source: source.to_string(),
        };
        let key: RowKey = (
            m.folder.clone(),
            m.drive_id.clone(),
            m.archived_time.clone(),
            m.project_no.clone(),
            m.in_drive_path.clone(),
            m.verify.clone(),
        );
        if seen.insert(key) {
            out.push(m);
        }
    }
    Ok(())
}

/// 读一个来源:能读则读、计入已检索;读失败只记为失败来源、绝不中断整次查询。
/// 本机索引"不存在"= 尚未归档(正常),不记失败;额外来源不存在则记失败。
/// 本机读失败(如被 Excel 占用打不开)也只记失败,其余来源仍能查。(review #4)
fn read_source(
    path: &Path,
    label: &str,
    is_main: bool,
    keyword: &str,
    out: &mut FindOutcome,
    seen: &mut HashSet<RowKey>,
) {
    if !path.is_file() {
        if !is_main {
            out.sources_failed.push(label.to_string());
        }
        return;
    }
    match read_catalog(path, label, keyword, &mut out.matches, seen) {
        Ok(()) => out.sources_searched += 1,
        Err(_) => out.sources_failed.push(label.to_string()),
    }
}

/// 在「本机总索引 + 配置的额外索引」里按关键词(文件夹名或编号 contains)查项目。
/// 任一来源读取失败都只记入 `sources_failed`、不让整次查询失败。
pub fn search(cfg: &Config, keyword: &str) -> Result<FindOutcome> {
    let mut out = FindOutcome::default();
    let mut seen: HashSet<RowKey> = HashSet::new();

    let main = paths::system_global_catalog(&cfg.system_root);
    read_source(&main, LOCAL_SOURCE, true, keyword, &mut out, &mut seen);

    for p in &cfg.extra_catalogs {
        let label = catalog_label(p);
        read_source(p, &label, false, keyword, &mut out, &mut seen);
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
        assert_eq!(r.matches[0].source, LOCAL_SOURCE);
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
            .any(|m| m.source == LOCAL_SOURCE && m.drive_id == "A备份2"));
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

    #[test]
    fn search_keeps_distinct_rows_differing_only_by_date() {
        // review #1:盘号/编号默认每机各自从 1 编,两台机器可能出现同名/同盘号/同编号
        // 但实为不同记录(此处仅备份时间不同)。绝不能被误并丢掉。
        let d = tempfile::tempdir().unwrap();
        let cfg_base = cfg_with_catalog(
            d.path(),
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             001proj,备份1,2026-01-01,001,项目\\001proj,SHA256-OK\n",
        );
        let other = d.path().join("B.csv");
        std::fs::write(
            &other,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             001proj,备份1,2026-05-29,001,项目\\001proj,SHA256-OK\n",
        )
        .unwrap();
        let cfg = Config {
            extra_catalogs: vec![other],
            ..cfg_base
        };
        let r = search(&cfg, "001proj").unwrap();
        assert_eq!(r.matches.len(), 2, "仅日期不同的两条不应被误并");
    }

    #[test]
    fn search_flags_unrecognized_extra_format_as_failed() {
        // review #6:选错文件(表头不是 bftool 索引)应记为"失败来源",而不是悄悄返回 0 条。
        let d = tempfile::tempdir().unwrap();
        let cfg_base = cfg_with_catalog(
            d.path(),
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             proj,备份1,2026-05-29,001,项目\\001,SHA256-OK\n",
        );
        let wrong = d.path().join("wrong.csv");
        std::fs::write(&wrong, "name,age\nfoo,1\n").unwrap();
        let cfg = Config {
            extra_catalogs: vec![wrong],
            ..cfg_base
        };
        let r = search(&cfg, "proj").unwrap();
        assert_eq!(r.matches.len(), 1, "本机仍查到");
        assert_eq!(r.sources_searched, 1, "无法识别的来源不计入已检索");
        assert_eq!(r.sources_failed, vec!["wrong".to_string()], "记为失败来源");
    }

    #[test]
    fn catalog_label_uses_parent_when_default_filename() {
        // review #2:默认文件名时用所在文件夹名区分,避免多份都叫"备份索引名单"。
        assert_eq!(catalog_label(Path::new("D:/汇总/A/备份索引名单.csv")), "A");
        assert_eq!(catalog_label(Path::new("D:/汇总/B/备份索引名单.csv")), "B");
        // 改过名的直接用文件名 stem
        assert_eq!(catalog_label(Path::new("D:/汇总/客厅台式.csv")), "客厅台式");
    }

    #[test]
    fn catalog_label_edge_cases() {
        // F3:盘符根 / 纯文件名(无父目录)等 edge case。

        // 纯文件名(改过名,无父目录)→ 直接用 stem,不应 panic。
        assert_eq!(
            catalog_label(Path::new("备份索引名单名单.csv")),
            "备份索引名单名单"
        );

        // 纯文件名 == 默认文件名:parent 为 ""(无可用文件夹名)→ 回退到 stem 本身,
        // 不会因为"想用父目录区分"而拿到空串。
        assert_eq!(
            catalog_label(Path::new("备份索引名单.csv")),
            default_catalog_stem()
        );

        // 默认文件名直接落在盘符根:父目录是 "D:\\",file_name 为空 → 同样回退到 stem。
        let at_root = catalog_label(Path::new("D:\\备份索引名单.csv"));
        assert_eq!(at_root, default_catalog_stem());
    }
}
