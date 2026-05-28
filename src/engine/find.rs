//! 根据关键词在全局索引(`备份索引名单.csv`)里找项目落到了哪块盘。

use anyhow::{Context, Result};
use std::fs;

use crate::config::Config;
use crate::engine::paths;

pub fn run(cfg: &Config, keyword: &str) -> Result<()> {
    let cat = paths::system_global_catalog(&cfg.system_root);
    if !cat.is_file() {
        println!("总索引还不存在（{}）—— 尚未归档过任何项目。", cat.display());
        return Ok(());
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

    let mut count = 0usize;
    println!(
        "{:<28}{:<10}{:<20}{:<8}{:<24}校验",
        "文件夹名", "备份盘", "备份时间", "编号", "盘内路径"
    );
    println!("{}", "-".repeat(110));
    for rec in rdr.records().flatten() {
        let folder = i_folder.and_then(|c| rec.get(c)).unwrap_or("");
        let no = i_no.and_then(|c| rec.get(c)).unwrap_or("");
        if !folder.contains(keyword) && !no.contains(keyword) {
            continue;
        }
        count += 1;
        println!(
            "{:<28}{:<10}{:<20}{:<8}{:<24}{}",
            folder,
            i_drive.and_then(|c| rec.get(c)).unwrap_or(""),
            i_time.and_then(|c| rec.get(c)).unwrap_or(""),
            no,
            i_path.and_then(|c| rec.get(c)).unwrap_or(""),
            i_check.and_then(|c| rec.get(c)).unwrap_or(""),
        );
    }
    println!("{}", "-".repeat(110));
    println!("匹配 {} 行。", count);
    Ok(())
}
