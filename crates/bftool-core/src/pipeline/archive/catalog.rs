//! 本盘/全局索引 CSV 追加与查询。
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct DriveCatalogRow {
    #[serde(rename = "ProjectNo")]
    pub(super) project_no: String,
    #[serde(rename = "ProjectName")]
    pub(super) project_name: String,
    #[serde(rename = "FileCount")]
    pub(super) file_count: u64,
    #[serde(rename = "TotalBytes")]
    pub(super) total_bytes: u64,
    #[serde(rename = "ArchivedUTC")]
    pub(super) archived_utc: String,
    #[serde(rename = "VerifyStatus")]
    pub(super) verify_status: String,
    #[serde(rename = "Status")]
    pub(super) status: String,
    #[serde(rename = "Notes")]
    pub(super) notes: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct GlobalCatalogRow {
    #[serde(rename = "文件夹名")]
    pub(super) folder_name: String,
    #[serde(rename = "备份盘名")]
    pub(super) drive_name: String,
    #[serde(rename = "备份时间")]
    pub(super) archived_time: String,
    #[serde(rename = "编号")]
    pub(super) project_no: String,
    #[serde(rename = "盘内路径")]
    pub(super) in_drive_path: String,
    #[serde(rename = "文件数")]
    pub(super) file_count: u64,
    #[serde(rename = "大小GB")]
    pub(super) size_gb: f64,
    #[serde(rename = "校验方式")]
    pub(super) verify: String,
    #[serde(rename = "校验清单")]
    pub(super) manifest_path: String,
}

/// 索引追加的幂等判定:对**已读入的现有内容**判断该行是否已存在(命中则跳过追加)。
type DedupCheck<'a> = dyn Fn(&[u8]) -> Result<bool> + 'a;

pub(super) fn append_drive_catalog(path: &Path, row: &DriveCatalogRow) -> Result<()> {
    let dedup = |content: &[u8]| {
        if content.is_empty() {
            return Ok(false);
        }
        let mut rdr = csv::Reader::from_reader(content);
        for old in rdr.deserialize::<DriveCatalogRow>() {
            let old = old?;
            if catalog_name_eq(&old.project_name, &row.project_name) {
                anyhow::ensure!(
                    old == *row,
                    "本盘索引已存在不同提交记录，拒绝覆盖或冒充完成"
                );
                return Ok(true);
            }
        }
        Ok(false)
    };
    append_catalog_row(path, row, Some(&dedup))
}

pub(super) fn append_global_catalog(path: &Path, row: &GlobalCatalogRow) -> Result<()> {
    // Exact replay of the same committed row is idempotent. A different row using the same
    // name/drive must never be silently treated as this transaction's completed metadata.
    let dedup = |content: &[u8]| {
        if content.is_empty() {
            return Ok(false);
        }
        let mut rdr = csv::Reader::from_reader(content);
        for old in rdr.deserialize::<GlobalCatalogRow>() {
            let old = old?;
            if catalog_name_eq(&old.folder_name, &row.folder_name)
                && catalog_name_eq(&old.drive_name, &row.drive_name)
            {
                anyhow::ensure!(old == *row, "全局索引已存在不同提交记录，拒绝冒充完成");
                return Ok(true);
            }
        }
        Ok(false)
    };
    append_catalog_row(path, row, Some(&dedup))
}

/// A historical name is occupied even if its payload is currently unavailable. This is name
/// collision protection only; it never certifies the current source or authorizes resume.
pub(super) fn global_catalog_has_project_on_drive(
    path: &Path,
    name: &str,
    drive: &str,
) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(path)?;
    let headers = rdr.headers()?.clone();
    let folder_column = headers
        .iter()
        .position(|h| h == "文件夹名")
        .context("全局索引缺少文件夹名列")?;
    let drive_column = headers
        .iter()
        .position(|h| h == "备份盘名")
        .context("全局索引缺少备份盘名列")?;
    for row in rdr.records() {
        let row = row?;
        if row
            .get(folder_column)
            .is_some_and(|v| catalog_name_eq(v, name))
            && row
                .get(drive_column)
                .is_some_and(|v| catalog_name_eq(v, drive))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 把一行追加进 CSV 索引并**原子落盘**:读现有内容到内存 → 追加序列化的新行(文件不存在时
/// 连表头一起)→ SafeDir 唯一临时文件 + fsync + 原子发布。
///
/// 旧实现用 `OpenOptions::append` + fsync,断电恰发生在"扇区已分配、字节未写完"时会在 CSV 末尾
/// 留下半行,让后续 catalog_has_project / global_has_folder / next_drive_number / seal 的解析
/// fail-closed(跳过项目/恢复 bail/选号失败),需用户手动修 CSV。原子 rename 保证读者只看到
/// 旧索引或完整新索引,绝不半行 —— 与 manifest/事务标记的原子写一致。(review-r2 R2-4 / L-001)
///
/// AR-10:`file_existed` 探测与写入之间的 TOCTOU 在单进程串行归档下可接受(无并发写者)。
///
/// `dedup`:可选幂等判定,对**已读入的现有内容**判断该行是否已存在,命中则跳过追加(复用这次读,
/// 不再单独读盘)。全局索引用它做(文件夹名,盘名)幂等;本盘索引传 None。
pub(super) fn append_catalog_row<R: serde::Serialize>(
    path: &Path,
    row: &R,
    dedup: Option<&DedupCheck>,
) -> Result<()> {
    let directory =
        crate::engine::destination::SafeDir::open(path.parent().context("索引缺少父目录")?, true)?;
    let existed = path.is_file();
    let mut content = if existed {
        fs::read(path).with_context(|| format!("读索引失败：{}", path.display()))?
    } else {
        Vec::new()
    };
    if let Some(dedup) = dedup {
        if dedup(&content)? {
            return Ok(()); // 已存在 → 幂等跳过(复用上面这次读,不重复读盘)
        }
    }
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(!existed) // 文件不存在时连表头一起写
        .from_writer(Vec::new());
    wtr.serialize(row)?;
    wtr.flush()?;
    let row_bytes = wtr
        .into_inner()
        .map_err(|e| anyhow::anyhow!("序列化索引行失败：{}", e))?;
    content.extend_from_slice(&row_bytes);
    directory
        .write_atomic(
            Path::new(path.file_name().context("索引缺少文件名")?),
            &content,
        )
        .with_context(|| format!("写索引失败：{}", path.display()))?;
    Ok(())
}

pub(super) fn catalog_has_project(catalog: &Path, project_name: &str) -> Result<bool> {
    if !catalog.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(catalog)?;
    let headers = rdr.headers()?.clone();
    // 文件存在且能作为 CSV 打开,但缺 ProjectName 列 → 索引不可信(损坏/被改格式),不能当作「空索引/未登记」
    // 静默返回 Ok(false)(那会让恢复判定把『已索引』误判为『未索引』→ 重做产生重复副本/漏判重名)。
    // 与 CSV 解析失败的 fail-closed 一致:bail,让调用方走既有 skip+note_manual / 上抛路径。(review-r3 round5)
    let Some(col) = headers.iter().position(|h| h == "ProjectName") else {
        anyhow::bail!(
            "本盘索引缺少 ProjectName 列,索引不可信(可能损坏或被改格式):{}",
            catalog.display()
        );
    };
    // NTFS 大小写不敏感:仅大小写不同的同名也算已存在,否则归档侧重名闸漏判 → 会尝试写进既有目录
    // (diff 兜底 fail-closed,但旧备份独有文件会被挪进隔离区降级)。与 verify::extras_key / safety 的
    // to_lowercase 折叠口径一致;非 Windows 保持精确比较。(强优化 review)
    let want = if cfg!(windows) {
        project_name.to_lowercase()
    } else {
        project_name.to_string()
    };
    for rec in rdr.records() {
        let rec = rec?;
        let hit = rec
            .get(col)
            .map(|v| {
                if cfg!(windows) {
                    v.to_lowercase() == want
                } else {
                    v == want
                }
            })
            .unwrap_or(false);
        if hit {
            return Ok(true);
        }
    }
    Ok(false)
}

fn catalog_name_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}
