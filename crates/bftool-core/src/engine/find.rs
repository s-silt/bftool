//! 根据关键词在全局索引(`备份索引名单.csv`)里找项目落到了哪块盘。
//! 支持多机汇总：本机总索引 + 配置里 `extra_catalogs` 的其它电脑索引一并检索。
//! core 提供结构化 `search`(GUI 直接消费),CLI `run` 负责渲染表格。(Spec D §4.1 / L-031)

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::path::Path;

use crate::config::Config;
use crate::engine::paths;

/// 本机总索引来源在结果里的来源标签。源码与测试统一引用,避免中文字面量重复。(F5)
pub const LOCAL_SOURCE: &str = "本机";

/// 单个索引 CSV 的大小上限。多机汇总会读其它电脑的索引(半信任输入),
/// `fs::read` 全量载入前先按此上限拦截,避免超大/畸形文件撑爆内存。(VulnGym 审计 CSV-1)
const MAX_CATALOG_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB

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
    /// 读取失败的来源:`(标签, 失败原因摘要)`。额外来源不存在、或任何来源读取/解析失败都计入;
    /// 本机索引**不存在**不算失败(=尚未归档),但本机索引存在却读取失败会计入。保留具体原因(过大/
    /// 损坏/选错文件/被占用)而非只记标签,供 CLI/GUI 给出可诊断提示。(review-r3 round3)
    pub sources_failed: Vec<(String, String)>,
    /// 跨所有成功读取来源累加的「格式错误被跳过」行数。>0 说明部分行未解析进结果,应提示用户。(CSV-2)
    pub malformed_rows: usize,
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
    max_bytes: u64,
) -> Result<usize> {
    // CSV-1:流式硬上限读取,消除「metadata 快照 → fs::read」之间的 TOCTOU —— metadata 取值后
    // 文件可能被写大(半信任的他机索引可能在网络盘上),fs::read 会按当前真实大小全量载入绕过闸。
    // take(max_bytes+1) 把载入内存的字节硬性封顶:读到 >max_bytes 即判过大。(review-r3 #13)
    let f = fs::File::open(path).with_context(|| format!("读索引失败：{}", path.display()))?;
    let mut bytes = Vec::new();
    let read = f
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("读索引失败：{}", path.display()))?;
    if read as u64 > max_bytes {
        anyhow::bail!(
            "索引文件过大（超过上限 {} 字节），已跳过：{}",
            max_bytes,
            path.display()
        );
    }
    let mut rdr = csv::Reader::from_reader(std::io::Cursor::new(bytes));
    let headers = rdr
        .headers()
        .with_context(|| format!("读索引表头失败：{}", path.display()))?
        .clone();
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

    let mut malformed = 0usize;
    let mut valid = 0usize;
    for result in rdr.records() {
        let rec = match result {
            Ok(r) => {
                valid += 1;
                r
            }
            // CSV-2:坏行(列数不符等)不再被 flatten() 静默吞,计数后跳过,供调用方提示用户。
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
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
    // CSV-3:表头可识别但正文有数据行且**全部**解析失败(valid==0 && malformed>0)→ 整份索引
    // 不可用,报错让 read_source 计入 sources_failed,而非伪装成"成功检索 1 个来源"误导用户
    // 误判"查不到=没备份"。空表(0 数据行)仍视为成功来源,不误报。(review-r3 #14)
    if valid == 0 && malformed > 0 {
        anyhow::bail!(
            "索引正文整体无法解析（{} 行全部格式错误,可能损坏或分隔符错误）：{}",
            malformed,
            path.display()
        );
    }
    Ok(malformed)
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
    max_bytes: u64,
) {
    if !path.is_file() {
        if !is_main {
            out.sources_failed
                .push((label.to_string(), "文件不存在或不是普通文件".to_string()));
        }
        return;
    }
    match read_catalog(path, label, keyword, &mut out.matches, seen, max_bytes) {
        Ok(bad) => {
            out.sources_searched += 1;
            out.malformed_rows += bad;
        }
        // 保留 read_catalog 精心构造的具体错误链(过大/格式无法识别/表头读失败/整体坏行/被占用),
        // 不再用 Err(_) 整条丢弃 → CLI/GUI 才能告诉用户失败的真正原因。(review-r3 round3)
        Err(e) => out
            .sources_failed
            .push((label.to_string(), format!("{e:#}"))),
    }
}

/// 把失败来源渲染成「标签:原因」串,供 CLI/GUI 统一展示。
pub fn render_failed_sources(failed: &[(String, String)]) -> String {
    failed
        .iter()
        .map(|(label, reason)| format!("{label}:{reason}"))
        .collect::<Vec<_>>()
        .join("、")
}

/// 在「本机总索引 + 配置的额外索引」里按关键词(文件夹名或编号 contains)查项目。
/// 任一来源读取失败都只记入 `sources_failed`、不让整次查询失败。
pub fn search(cfg: &Config, keyword: &str) -> Result<FindOutcome> {
    let mut out = FindOutcome::default();
    let mut seen: HashSet<RowKey> = HashSet::new();

    let main = paths::system_global_catalog(&cfg.system_root);
    read_source(
        &main,
        LOCAL_SOURCE,
        true,
        keyword,
        &mut out,
        &mut seen,
        MAX_CATALOG_BYTES,
    );

    for p in &cfg.extra_catalogs {
        let label = catalog_label(p);
        read_source(
            p,
            &label,
            false,
            keyword,
            &mut out,
            &mut seen,
            MAX_CATALOG_BYTES,
        );
    }

    Ok(out)
}

/// sources_searched==0 时的提示文案。纯函数,可测。
/// 关键区分:本机索引**存在但读失败**(被 Excel 占用/损坏)≠ 尚未归档 —— 否则把"读不出"
/// 误报成"没有",用户以为备份记录丢了。(review-r2 R4-6)
fn empty_result_message(outcome: &FindOutcome) -> String {
    let local_failed = outcome
        .sources_failed
        .iter()
        .any(|(label, _)| label == LOCAL_SOURCE);
    if local_failed {
        let others: Vec<(String, String)> = outcome
            .sources_failed
            .iter()
            .filter(|(label, _)| label != LOCAL_SOURCE)
            .cloned()
            .collect();
        let mut msg =
            "本机索引存在但读取失败(可能被 Excel 等程序占用,或文件损坏)——请关闭占用程序/修复后重试。"
                .to_string();
        if !others.is_empty() {
            msg.push_str(&format!(
                " 另有 {} 个额外索引来源读取失败:{}",
                others.len(),
                render_failed_sources(&others)
            ));
        }
        msg
    } else if outcome.sources_failed.is_empty() {
        "还没有任何可检索的索引 —— 尚未归档过任何项目，也没有配置额外索引来源。".to_string()
    } else {
        format!(
            "没有可检索的索引：本机尚未归档，且 {} 个额外索引来源都读取失败：{}",
            outcome.sources_failed.len(),
            render_failed_sources(&outcome.sources_failed)
        )
    }
}

pub fn run(cfg: &Config, keyword: &str) -> Result<()> {
    // 空/纯空白关键词会 contains-匹配所有行(等于"列全部"),且与 GUI(已拒空)行为不一致 → 拒掉。(review-r2 R5-4)
    if keyword.trim().is_empty() {
        anyhow::bail!("请输入查找关键词(项目名或编号片段);留空不会列出全部项目。");
    }
    let outcome = search(cfg, keyword)?;
    if outcome.sources_searched == 0 {
        println!("{}", empty_result_message(&outcome));
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
            "注意：{} 个索引来源读取失败，已跳过：{}",
            outcome.sources_failed.len(),
            render_failed_sources(&outcome.sources_failed)
        );
    }
    if outcome.malformed_rows > 0 {
        println!(
            "注意：{} 行因格式错误被跳过（未计入结果，可能是索引文件损坏或列不齐）。",
            outcome.malformed_rows
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

    // ── review-r2 R5-4:CLI find 空/纯空白关键词被拒(不列出全部) ──
    #[test]
    fn run_rejects_empty_keyword() {
        assert!(run(&Config::default(), "").is_err(), "空关键词应被拒");
        assert!(
            run(&Config::default(), "   ").is_err(),
            "纯空白关键词应被拒"
        );
    }

    // ── review-r2 R4-6:本机索引读失败(被占用/损坏)不能误报为"尚未归档" ──
    #[test]
    fn empty_result_message_distinguishes_local_read_failure() {
        // 本机索引存在但读失败 → sources_failed 含 LOCAL_SOURCE
        let mut o = FindOutcome::default();
        o.sources_failed
            .push((LOCAL_SOURCE.to_string(), "读取失败".to_string()));
        let msg = empty_result_message(&o);
        assert!(
            msg.contains("读取失败"),
            "应提示读取失败而非尚未归档;实际:{msg}"
        );
        assert!(
            !msg.contains("尚未归档过任何项目"),
            "不应误报尚未归档;实际:{msg}"
        );
        // 真正尚未归档(无失败来源)→ 仍提示尚未归档
        let empty = FindOutcome::default();
        assert!(empty_result_message(&empty).contains("尚未归档过任何项目"));
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
        assert_eq!(r.sources_failed[0].0, "没有", "失败来源标签");
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
        assert_eq!(r.sources_failed.len(), 1, "记为失败来源");
        assert_eq!(r.sources_failed[0].0, "wrong", "失败来源标签");
        assert!(
            r.sources_failed[0].1.contains("无法识别"),
            "失败原因应保留 read_catalog 的具体错误(不再 Err(_) 丢弃),实际:{}",
            r.sources_failed[0].1
        );
    }

    // ── CSV-2(VulnGym 审计):坏行不再被 flatten() 静默吞,而是计数 ──
    #[test]
    fn read_catalog_counts_malformed_rows() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.csv");
        // 表头 6 列;第二数据行列数不符 → csv 解析 Err(非 flexible)。
        std::fs::write(
            &p,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             proj,备份1,2026-05-29,001,项目\\001,SHA256-OK\n\
             badrow,只有两列\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let malformed =
            read_catalog(&p, "src", "proj", &mut out, &mut seen, MAX_CATALOG_BYTES).unwrap();
        assert_eq!(out.len(), 1, "好行仍命中");
        assert_eq!(malformed, 1, "坏行被计数而非静默吞");
    }

    // ── review-r3 #14:表头可识别但正文有数据行且全部解析失败 → 整份不可用(Err),
    // 而非伪装成"成功检索 1 个来源" ──
    #[test]
    fn read_catalog_all_rows_malformed_is_err() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.csv");
        // 表头 6 列;两条数据行列数都不符 → 全部解析失败,valid==0。
        std::fs::write(
            &p,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             bad1,只有两列\n\
             bad2,也两列\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let r = read_catalog(&p, "src", "proj", &mut out, &mut seen, MAX_CATALOG_BYTES);
        assert!(r.is_err(), "正文全为坏行的索引应判失败(不可用),不应 Ok");
    }

    // ── review-r3 #14:空表(仅表头、无数据行)仍视为成功来源,不误判失败 ──
    #[test]
    fn read_catalog_empty_table_is_ok() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.csv");
        std::fs::write(&p, "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n").unwrap();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let malformed =
            read_catalog(&p, "src", "proj", &mut out, &mut seen, MAX_CATALOG_BYTES).unwrap();
        assert_eq!(malformed, 0, "空表无坏行");
        assert!(out.is_empty(), "空表无命中");
    }

    // ── CSV-1(VulnGym 审计):超过大小上限的索引文件直接判失败,不全量载入内存 ──
    #[test]
    fn read_catalog_rejects_oversized_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.csv");
        std::fs::write(&p, "文件夹名,编号\nproj,001\n").unwrap(); // 远超 5 字节
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let r = read_catalog(&p, "src", "proj", &mut out, &mut seen, 5);
        assert!(r.is_err(), "超出 max_bytes 上限应返回 Err");
    }

    // ── 端到端:本机索引坏行计数汇入 outcome.malformed_rows ──
    #[test]
    fn search_reports_malformed_rows() {
        let d = tempfile::tempdir().unwrap();
        let cfg = cfg_with_catalog(
            d.path(),
            "文件夹名,备份盘名,备份时间,编号,盘内路径,校验方式\n\
             proj,备份1,2026-05-29,001,项目\\001,SHA256-OK\n\
             bad,x\n",
        );
        let r = search(&cfg, "proj").unwrap();
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.malformed_rows, 1, "坏行计数汇入 outcome");
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
        #[cfg(windows)]
        let root_file = Path::new("D:\\备份索引名单.csv");
        #[cfg(not(windows))]
        let root_file = Path::new("/备份索引名单.csv");
        let at_root = catalog_label(root_file);
        assert_eq!(at_root, default_catalog_stem());
    }
}
