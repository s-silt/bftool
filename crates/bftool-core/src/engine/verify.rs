//! 复查：对一块盘上每个项目重算 SHA256，比对清单；同时报告"清单外多余文件"。
//! 借鉴 restic check / conserve validate 的理念：完整性检查要覆盖 数据 + 元数据。

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::config::Config;
use crate::engine::{drive, paths};
use crate::ui;

pub fn run(cfg: &Config, drive_letter: Option<&str>) -> Result<()> {
    let drives = drive::scan_mounted()?;
    if drives.is_empty() {
        bail!("未发现已初始化的备份盘。");
    }
    let target = match drive_letter {
        Some(l) => {
            let letter = l.trim_end_matches(':').to_uppercase();
            drives
                .into_iter()
                .find(|d| d.letter == letter)
                .ok_or_else(|| {
                    anyhow::anyhow!("找不到盘 {}:（用 `bftool drives` 看一下）", letter)
                })?
        }
        None => {
            if drives.len() == 1 {
                drives.into_iter().next().unwrap()
            } else {
                ui::info("检测到多块备份盘；请显式指定盘符。已识别：");
                for (i, d) in drives.iter().enumerate() {
                    println!("  [{}] {} ({}:)", i + 1, d.id, d.letter);
                }
                bail!("用法：bftool verify <盘符>（例：bftool verify E）");
            }
        }
    };

    let mdir = paths::drive_manifest_dir(&target.root);
    if !mdir.is_dir() {
        bail!("盘 {} 上没有校验清单目录：{}", target.id, mdir.display());
    }
    ui::info(format!(
        "开始复查 {} ({}:)，重算 SHA256 / 核对大小，可能较慢…",
        target.id, target.letter
    ));

    let mut checked: u64 = 0;
    let mut bad: u64 = 0;
    let mut extra: u64 = 0;

    let mut manifest_files: Vec<_> = fs::read_dir(&mdir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sha256.csv"))
        .collect();
    manifest_files.sort_by_key(|e| e.file_name());

    let projects_dir = paths::drive_projects_dir(&target.root);
    let _ = cfg; // 当前 verify 不需要 cfg；保留参数便于将来加 --full 整库交叉核对

    for mf in &manifest_files {
        let stem = mf.file_name();
        let stem = stem.to_string_lossy();
        let project_name = stem
            .strip_suffix(".sha256.csv")
            .unwrap_or(&stem)
            .to_string();
        ui::action(format!("· {}", project_name));
        let proj_dir = projects_dir.join(&project_name);
        let mut expected: HashMap<String, (u64, String)> = HashMap::new();

        let mut rdr = csv::Reader::from_path(mf.path())
            .with_context(|| format!("读校验清单失败：{}", mf.path().display()))?;
        let headers = rdr.headers()?.clone();
        let i_rel = headers.iter().position(|h| h == "Rel");
        let i_size = headers.iter().position(|h| h == "Size");
        let i_hash = headers.iter().position(|h| h == "Hash");
        let Some(i_rel) = i_rel else {
            ui::warn(format!("清单缺少 Rel 列，跳过：{}", mf.path().display()));
            continue;
        };

        for rec in rdr.records().flatten() {
            let rel = rec.get(i_rel).unwrap_or("").to_string();
            let size = i_size
                .and_then(|c| rec.get(c))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let hash = i_hash.and_then(|c| rec.get(c)).unwrap_or("").to_string();
            expected.insert(rel.clone(), (size, hash.clone()));

            let f = proj_dir.join(&rel);
            checked += 1;
            if !f.is_file() {
                ui::error(format!("  缺失: {}", rel));
                bad += 1;
                continue;
            }
            let meta = match fs::metadata(&f) {
                Ok(m) => m,
                Err(e) => {
                    ui::error(format!("  无法读元数据 {}: {}", rel, e));
                    bad += 1;
                    continue;
                }
            };
            if size > 0 && meta.len() != size {
                ui::error(format!("  大小不一致: {}", rel));
                bad += 1;
                continue;
            }
            if !hash.is_empty() {
                match sha256_hex(&f) {
                    Ok(h) if h == hash => {}
                    Ok(_) => {
                        ui::error(format!("  损坏/不一致: {}", rel));
                        bad += 1;
                    }
                    Err(e) => {
                        ui::error(format!("  读取失败 {}: {}", rel, e));
                        bad += 1;
                    }
                }
            }
        }

        // 报告清单外的多余文件（不删除）
        if proj_dir.is_dir() {
            let base = proj_dir.canonicalize().unwrap_or(proj_dir.clone());
            for entry in walkdir::WalkDir::new(&proj_dir).follow_links(false) {
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&base)
                    .map(|p| p.to_string_lossy().replace('/', "\\"))
                    .unwrap_or_default();
                if !expected.contains_key(&rel) {
                    ui::warn(format!("  多余(清单外): {}", rel));
                    extra += 1;
                }
            }
        }
    }

    let summary = format!(
        "复查完成：检查 {} 个文件，损坏/缺失/大小问题 {} 个，清单外多余 {} 个。",
        checked, bad, extra
    );
    if bad > 0 {
        ui::error(summary);
    } else if extra > 0 {
        ui::warn(summary);
    } else {
        ui::ok(summary);
    }
    if extra > 0 {
        ui::info("（多余文件不会自动删除；如确认无用可人工清理。）");
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String> {
    let f = std::fs::File::open(path)?;
    let mut reader = BufReader::with_capacity(1024 * 1024, f);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write as _;
        write!(&mut s, "{:02X}", b).ok();
    }
    Ok(s)
}
