//! 路径安全检查 + 文件夹稳定性检测。

use anyhow::{bail, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::engine::cruft;

/// 校验三个根目录之间的相互关系，以及它们不在备份盘上。
pub fn check_paths(
    ready: &Path,
    archived: &Path,
    system: &Path,
    drive_root: Option<&Path>,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let r = canon(ready);
    let a = canon(archived);
    let s = canon(system);

    // 已知局限(review-r3 round4):根目录本身是 junction/symlink 时,canon 的 canonicalize 会跟随它,
    // is_inside/qualifier 基于解析后路径,对链接重定向的嵌套/同卷判定保护有限。这里只在加载阶段对
    // 「根目录本身是 reparse point」发显式提醒,让该已知局限对用户可见(不改判定逻辑本身)。
    for (name, p) in [
        ("待备份", ready),
        ("已备份", archived),
        ("备份系统", system),
    ] {
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            if meta.file_type().is_symlink() {
                warnings.push(format!(
                    "提醒:{}({}) 本身是符号链接/junction —— 路径关系与同卷安全判定对链接重定向的保护有限,建议改用真实目录。",
                    name,
                    p.display()
                ));
            }
        }
    }

    // 三个根目录两两之间既不能相等、也不能互相嵌套(任一方向)。
    // 旧实现只查了 ready==archived、a/s 嵌套在 r、r 嵌套在 a —— 漏了
    // system 与 ready/archived「完全相等」(is_inside 在相等时返回 false 拦不住)
    // 以及 system↔archived 的双向嵌套。误配会把系统文件(全局索引/事务标记/日志)
    // 写进已备份或待备份树,污染备份/索引。改为对 (r,a,s) 做全覆盖的两两校验。(review-r3)
    let dirs: [(&str, &Path); 3] = [
        ("待备份", r.as_path()),
        ("已备份", a.as_path()),
        ("备份系统", s.as_path()),
    ];
    for i in 0..3 {
        for j in (i + 1)..3 {
            if eq_ci(dirs[i].1, dirs[j].1) {
                bail!(
                    "{} 与 {} 不能是同一目录（{}）。",
                    dirs[i].0,
                    dirs[j].0,
                    dirs[i].1.display()
                );
            }
        }
    }
    for i in 0..3 {
        for j in 0..3 {
            if i == j {
                continue;
            }
            if is_inside(dirs[i].1, dirs[j].1) {
                // 位于 待备份(索引 0) 之内的任何目录都会被 discover 当作待归档项目。
                let extra = if j == 0 {
                    "，否则会被当作待归档项目处理"
                } else {
                    ""
                };
                bail!(
                    "{}({}) 不能位于 {}({}) 之内{}。",
                    dirs[i].0,
                    dirs[i].1.display(),
                    dirs[j].0,
                    dirs[j].1.display(),
                    extra
                );
            }
        }
    }

    if let Some(drive) = drive_root {
        // 备份盘根在生产中总是卷根(如 `E:\`):此时「根目录在备份盘上」= 同卷(比盘符)。
        // 但若传入的是某个子目录(测试夹具或特殊配置),按盘符比会把"同卷不同目录"误判 ——
        // 改为看「根目录是否就在该目录之内」。两种情形都能正确拦住"把源/索引写到备份盘自身"。(review-r2 #1)
        let drive_c = canon(drive);
        let drive_q = qualifier(&drive_c);
        let drive_is_volume = is_volume_root(&drive_c);
        for (name, p) in [("待备份", &r), ("已备份", &a), ("备份系统", &s)] {
            let on_backup = if drive_is_volume {
                !drive_q.is_empty() && qualifier(p).eq_ignore_ascii_case(&drive_q)
            } else {
                eq_ci(p, &drive_c) || is_inside(p, &drive_c)
            };
            if on_backup {
                bail!(
                    "{}({}) 位于当前备份盘 {} 上 —— 会把源/索引写到备份盘自身。请改到固态盘。",
                    name,
                    p.display(),
                    drive.display()
                );
            }
        }
    }

    if !qualifier(&r).eq_ignore_ascii_case(&qualifier(&a)) {
        warnings.push(format!(
            "提醒：待备份({}) 与 已备份({}) 不在同一分区 —— 跨卷无法原子移动源,归档到「移动源」这步会失败(项目留在 待备份、可重做)。建议把两者放在同一块盘。",
            r.display(),
            a.display()
        ));
    }
    Ok(warnings)
}

fn canon(p: &Path) -> PathBuf {
    // canonicalize 对**存在**路径给规范长名(展开 8.3 短名、去 verbatim);对**不存在**路径会失败、
    // 退回原样(可能含 8.3 短名或用户写的 `/`)。直接混用会让"存在的父"与"不存在的子"前缀不一致,
    // is_inside/eq_ci 漏判(本地无 8.3 看不出,但 CI runner 的 RUNNER~1 短名 temp 路径会触发)。
    // 解决:不存在时锚定到**最长存在的祖先**——canonicalize 该祖先,再把其余尾段拼回,
    // 保证存在/不存在路径共享同一规范前缀。(review-r2 #1/#4 + R3-4)
    let c = if let Ok(c) = p.canonicalize() {
        strip_verbatim(&c)
    } else {
        let mut anchored = None;
        for anc in p.ancestors() {
            if let Ok(c) = anc.canonicalize() {
                let mut base = strip_verbatim(&c);
                if let Ok(rest) = p.strip_prefix(anc) {
                    base.push(rest);
                }
                anchored = Some(base);
                break;
            }
        }
        anchored.unwrap_or_else(|| p.to_path_buf())
    };
    // 统一分隔符为 `\`(toml 里写 `/` 也能正确比较嵌套/同盘)。仅用于比较,不动真实路径。
    PathBuf::from(c.to_string_lossy().replace('/', "\\"))
}

/// 去掉 Windows `canonicalize()` 对**存在**路径加的 `\\?\`(及 `\\?\UNC\`)verbatim 前缀。
/// canon 对存在/不存在的路径分别返回 verbatim / 原样,前缀不一致会让 `is_inside` / `eq_ci` /
/// `qualifier` 在两者之间比较时错位(把嵌套判成不嵌套、同盘判成不同盘)。统一剥掉前缀即可。(review-r2 #1/#4)
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

fn eq_ci(a: &Path, b: &Path) -> bool {
    // 用 Unicode 全量折叠(to_lowercase),与 is_inside 保持一致。曾用 eq_ignore_ascii_case 只折叠
    // ASCII,导致仅在非 ASCII 字母大小写上不同的同一目录(如 D:\Été 与 D:\été,NTFS 视为同一)
    // 在「同目录」闸(check_paths)与「根目录在备份盘上」数据安全闸里被判不等而漏拦。(review-r3 round2)
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

fn is_inside(child: &Path, parent: &Path) -> bool {
    // 注意:canonicalize 在路径不存在时回退原始路径,junction/symlink 重定向场景下保护有限。
    // Windows 备份场景下此风险低(正常用户不会故意构建 junction 攻击自己),已知局限。
    // 比较前把两侧规范化为「恰好一个尾分隔符」:父目录本就以 `\` 结尾时(盘根 D:\、或 toml 写的带
    // 尾斜杠根),直接追加会得到双反斜杠 `d:\\`,无法成为 `d:\child\` 的前缀 → 嵌套漏判、安全闸被绕过
    // (如 ready=D:\、archived=D:\Archived 时『已备份在待备份内』本应 bail 却放行)。
    // 先 trim_end_matches('\\') 再统一补一个 `\`。(review-r3 round3)
    let norm = |s: &str| format!("{}\\", s.trim_end_matches('\\'));
    let c = norm(&child.to_string_lossy().to_lowercase());
    let p = norm(&parent.to_string_lossy().to_lowercase());
    c.starts_with(&p) && c != p
}

/// `p` 是否是卷根(如 `E:\` / `\\?\E:\`)—— 即只有「盘符前缀 + 根」、无其它路径段。
/// 生产中备份盘根恒为卷根;据此区分"按盘符比同卷" vs"按目录包含比"。(review-r2 #1)
fn is_volume_root(p: &Path) -> bool {
    use std::path::Component;
    let mut comps = p.components();
    matches!(comps.next(), Some(Component::Prefix(_)))
        && matches!(comps.next(), Some(Component::RootDir))
        && comps.next().is_none()
}

fn qualifier(p: &Path) -> String {
    // 取卷标识:盘符 "D:" 或 UNC 卷 "\\SERVER\SHARE"(均大写归一化);无法识别时为 ""。
    // 字符安全:多字节首字符 / UNC 不 panic(旧 `&s[1..2]` 会)。(ledger L-023)
    // 注意:junction/symlink 仍可绕过此卷比较(已知局限,Windows 备份场景风险低)。
    let full = p.to_string_lossy();
    // 先剥掉 Windows `canonicalize()` 对**真实存在**路径加的 verbatim 前缀(`\\?\` / `\\?\UNC\`)——
    // 否则盘符/卷判空 → check_paths 的「根目录在备份盘上」安全闸与跨分区警告静默失效
    // (review-r2 #1/#4:canon 后的路径走这里,而 drive.root 未 canon,两边永不相等)。
    // `\\?\UNC\server\share\…` 还原成普通 UNC 形态 `\\server\share\…` 统一处理。(review-r3)
    let s: String = if let Some(rest) = full.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = full.strip_prefix(r"\\?\") {
        rest.to_string() // \\?\C:\... → C:\...
    } else {
        full.into_owned()
    };
    // UNC 路径(`\\server\share\…`):卷标识取 `\\server\share`(大小写归一化为大写)。
    // 旧实现一律返回空串,导致两个不同网络共享的 qualifier 都为 "" → 误判同卷,
    // 跨分区警告对 UNC 配置静默失效。(review-r3)
    if let Some(rest) = s.strip_prefix(r"\\") {
        let mut parts = rest.splitn(3, '\\');
        if let (Some(server), Some(share)) = (parts.next(), parts.next()) {
            if !server.is_empty() && !share.is_empty() {
                return format!(r"\\{}\{}", server.to_uppercase(), share.to_uppercase());
            }
        }
        return String::new();
    }
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), Some(':')) if c.is_ascii_alphabetic() => format!("{}:", c.to_ascii_uppercase()),
        _ => String::new(),
    }
}

#[derive(Debug)]
pub struct StableCheck {
    pub stable: bool,
    pub reason: String,
}

/// 文件夹是否稳定：所有文件最近修改时间须早于 N 分钟前，且不被占用。
pub fn folder_stable(root: &Path, minutes: u64) -> StableCheck {
    // minutes 用户可控(bftool.toml / --stable-minutes):saturating_mul 防溢出 panic,
    // checked_sub 防 SystemTime 下溢;极端值退化到 UNIX_EPOCH → 任何文件都比 cutoff 新 →
    // 保守判「未稳定」不归档(fail-closed),而非崩溃。(review-r2 R2-6)
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(minutes.saturating_mul(60)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for entry in cruft::walk(root) {
        // 稳定性检测对 walkdir 错误**保持原 swallow 语义**：
        // 真实枚举错误会在后续 manifest::real_files 阶段被收集并 bail。
        // 让稳定性检测也 bail 会让单个"权限拒绝"在第一关就把项目挡掉，
        // 用户看不到 manifest 阶段更详细的多文件错误汇总。
        let Ok(e) = entry else { continue };
        if !e.file_type().is_file() {
            continue;
        }
        // metadata()/modified() 读失败不再静默跳过时间窗判定:取不到修改时间 = 无法确认稳定 →
        // 保守判「未稳定」(fail-closed),与本闸的暂态语义一致(未稳定只是本轮跳过、下轮再来,不误备),
        // 也与下游 manifest/folder_stats 的 fail-closed 对齐,避免一个正在被写入但 metadata 瞬时
        // 读不到的文件被误判已稳定而提前归档。(review-r3 round4)
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(err) => {
                return StableCheck {
                    stable: false,
                    reason: format!(
                        "无法读元数据(保守判未稳定):{}({})",
                        e.file_name().to_string_lossy(),
                        err
                    ),
                }
            }
        };
        match meta.modified() {
            Ok(mt) if mt > cutoff => {
                return StableCheck {
                    stable: false,
                    reason: format!("最近被修改：{}", e.file_name().to_string_lossy()),
                }
            }
            Ok(_) => {}
            Err(err) => {
                return StableCheck {
                    stable: false,
                    reason: format!(
                        "无法读修改时间(保守判未稳定):{}({})",
                        e.file_name().to_string_lossy(),
                        err
                    ),
                }
            }
        }
        // 试着独占只读打开一下，发现被进程独占就跳过
        if let Err(err) = File::open(e.path()) {
            return StableCheck {
                stable: false,
                reason: format!(
                    "被占用/无法读取：{}({})",
                    e.file_name().to_string_lossy(),
                    err
                ),
            };
        }
    }
    StableCheck {
        stable: true,
        reason: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── L-023: qualifier 字符安全 ──
    #[test]
    fn qualifier_is_char_safe() {
        assert_eq!(qualifier(Path::new("D:\\x")), "D:");
        assert_eq!(qualifier(Path::new("e:\\")), "E:");
        assert_eq!(qualifier(Path::new(r"\\srv\share")), r"\\SRV\SHARE"); // UNC:卷标识
        assert_eq!(qualifier(Path::new("中:\\x")), ""); // 多字节首字符:不 panic
        assert_eq!(qualifier(Path::new("")), "");
    }

    // ── SEC verbatim(review-r2 #1/#4):canonicalize 给真实路径加 `\\?\` 前缀后,
    // qualifier 必须仍能取出盘符 —— 否则「根目录写到备份盘」安全闸与跨分区警告静默失效 ──
    #[test]
    fn qualifier_strips_verbatim_prefix() {
        // Windows canonicalize() 对**存在**的路径返回 `\\?\` verbatim 前缀(实测 C:\Windows → \\?\C:\Windows)。
        assert_eq!(qualifier(Path::new(r"\\?\C:\Windows")), "C:");
        assert_eq!(qualifier(Path::new(r"\\?\E:\ready")), "E:");
        // verbatim UNC 还原成卷标识(\\server\share),不再误判为空串
        assert_eq!(qualifier(Path::new(r"\\?\UNC\srv\share")), r"\\SRV\SHARE");
        // 普通盘符行为不变;普通 UNC 取卷标识
        assert_eq!(qualifier(Path::new(r"D:\x")), "D:");
        assert_eq!(qualifier(Path::new(r"\\srv\share")), r"\\SRV\SHARE");
        // 不同共享 → 不同卷标识(跨分区警告据此对 UNC 生效)
        assert_ne!(
            qualifier(Path::new(r"\\srvA\share1")),
            qualifier(Path::new(r"\\srvB\share2"))
        );
        // 同一共享下的子路径 → 同卷标识
        assert_eq!(
            qualifier(Path::new(r"\\srv\share\sub\a")),
            qualifier(Path::new(r"\\srv\share"))
        );
    }

    // ── L-026: is_inside 边界(前缀不能误判为嵌套) ──
    #[test]
    fn is_inside_basic() {
        assert!(is_inside(Path::new("D:\\a\\b"), Path::new("D:\\a")));
        assert!(!is_inside(Path::new("D:\\ab"), Path::new("D:\\a"))); // 前缀但非父目录
        assert!(!is_inside(Path::new("D:\\a"), Path::new("D:\\a"))); // 自身不算 inside
    }

    // ── review-r3 round3:父目录以 `\` 结尾(盘根/带尾斜杠)不得漏判嵌套 ──
    #[test]
    fn is_inside_handles_trailing_slash_parent() {
        assert!(
            is_inside(Path::new(r"D:\Archived"), Path::new(r"D:\")),
            "盘根 D:\\ 作父目录时其子目录应判在内(旧实现双反斜杠漏判)"
        );
        assert!(
            is_inside(Path::new(r"D:\lib\done"), Path::new(r"D:\lib\")),
            "带尾斜杠的父目录应正确判嵌套"
        );
        assert!(
            !is_inside(Path::new(r"D:\"), Path::new(r"D:\")),
            "盘根自身不算 inside"
        );
    }

    #[test]
    fn check_paths_rejects_subdir_of_drive_root() {
        // ready=整盘根、archived=盘根子目录:本应判「已备份在待备份内」并 bail。
        let r = check_paths(
            Path::new(r"D:\"),
            Path::new(r"D:\Archived"),
            Path::new(r"D:\Sys"),
            None,
        );
        assert!(
            r.is_err(),
            "ready=盘根、archived=其子目录 应被嵌套闸拒绝(否则盘根下一切会被当待归档项目移走)"
        );
    }

    // ── L-026: check_paths 拒绝同目录/嵌套/同备份盘 ──
    #[test]
    fn check_paths_rejects_same_ready_archived() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\sys"),
            None,
        );
        assert!(r.is_err(), "待备份==已备份 应拒绝");
    }

    #[test]
    fn check_paths_rejects_archived_inside_ready() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\ready\\done"),
            Path::new("D:\\lib\\sys"),
            None,
        );
        assert!(r.is_err(), "已备份 在 待备份 之内 应拒绝");
    }

    #[test]
    fn check_paths_rejects_root_on_backup_drive() {
        let r = check_paths(
            Path::new("E:\\ready"),
            Path::new("D:\\archived"),
            Path::new("D:\\sys"),
            Some(Path::new("E:\\")),
        );
        assert!(r.is_err(), "根目录在备份盘 E: 上 应拒绝");
    }

    // ── review-r2 #1 回归:真实**存在**的根目录会被 canonicalize 加 `\\?\` 前缀;
    // 安全闸必须仍拒绝。旧测试只用不存在路径(canon 回退原样)"假绿",掩盖了 verbatim bug。──
    #[cfg(windows)]
    #[test]
    fn check_paths_rejects_existing_root_on_backup_drive() {
        let d = tempfile::tempdir().unwrap();
        let ready = d.path().join("ready");
        let archived = d.path().join("archived");
        let system = d.path().join("system");
        std::fs::create_dir_all(&ready).unwrap();
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        // 备份盘根 = tempdir 所在盘 → 三根目录都"在备份盘上",应触发安全闸。
        let drive_letter = d.path().to_string_lossy().chars().next().unwrap();
        let drive_root = PathBuf::from(format!("{drive_letter}:\\"));
        let r = check_paths(&ready, &archived, &system, Some(&drive_root));
        assert!(
            r.is_err(),
            "真实存在的根目录在备份盘上 应被拒绝(verbatim 前缀不得绕过安全闸)"
        );
    }

    // ── L-010: 跨分区给出准确提醒(归档移源会失败,而非"复制+删除") ──
    #[test]
    fn check_paths_warns_on_different_partition() {
        let w = check_paths(
            Path::new("C:\\ready"),
            Path::new("D:\\archived"),
            Path::new("D:\\sys"),
            None,
        )
        .unwrap();
        assert!(
            w.iter().any(|s| s.contains("不在同一分区")),
            "跨分区应有提醒,实际:{:?}",
            w
        );
    }

    #[test]
    fn check_paths_ok_when_distinct_same_partition() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\archived"),
            Path::new("D:\\lib\\sys"),
            Some(Path::new("E:\\")),
        )
        .unwrap();
        // 同分区、互不嵌套 → Ok,且无"不同分区"警告
        assert!(r.is_empty(), "同分区互不嵌套应无警告,实际:{:?}", r);
    }

    // ── review-r2:canon 对不存在的子目录须锚定到存在父的规范名(防 8.3 短名/verbatim 漏判)──
    #[test]
    fn canon_anchors_nonexistent_to_existing_ancestor() {
        let d = tempfile::tempdir().unwrap();
        let parent = d.path().join("p");
        std::fs::create_dir_all(&parent).unwrap();
        let child = parent.join("不存在子");
        let cp = canon(&parent);
        let cc = canon(&child);
        // Join using this platform's native separator before converting the
        // comparison form to Windows-style separators. Joining an already
        // normalized comparison path adds '/' on Linux and gives a false failure.
        let expected = strip_verbatim(&parent.canonicalize().unwrap()).join("不存在子");
        let expected = PathBuf::from(expected.to_string_lossy().replace('/', "\\"));
        assert_eq!(cc, expected, "不存在子应锚定到存在父的规范名");
        assert!(is_inside(&cc, &cp), "不存在子应被判在父内");
    }

    // ── review-r2 R3-4:toml 里用正斜杠写的嵌套路径,安全闸不得漏判 ──
    #[test]
    fn check_paths_detects_nesting_with_forward_slashes() {
        let r = check_paths(
            Path::new("D:/lib/ready"),
            Path::new("D:/lib/ready/done"), // 已备份 在 待备份 内(正斜杠)
            Path::new("D:/lib/sys"),
            None,
        );
        assert!(r.is_err(), "正斜杠嵌套(已备份在待备份内)应被拒");
    }

    // ── review-r3:三根目录两两关系须全覆盖 —— system 与 ready/archived 相等、
    // 以及 system↔archived 双向嵌套,旧实现都漏了(is_inside 在相等时返回 false)──
    #[test]
    fn check_paths_rejects_same_system_archived() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\archived"),
            Path::new("D:\\lib\\archived"), // 备份系统 == 已备份
            None,
        );
        assert!(r.is_err(), "备份系统==已备份 应拒绝");
    }

    #[test]
    fn check_paths_rejects_same_system_ready() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\archived"),
            Path::new("D:\\lib\\ready"), // 备份系统 == 待备份
            None,
        );
        assert!(r.is_err(), "备份系统==待备份 应拒绝");
    }

    #[test]
    fn check_paths_rejects_system_inside_archived() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\archived"),
            Path::new("D:\\lib\\archived\\sys"), // 备份系统 在 已备份 之内
            None,
        );
        assert!(r.is_err(), "备份系统 在 已备份 内 应拒绝");
    }

    #[test]
    fn check_paths_rejects_archived_inside_system() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\sys\\archived"), // 已备份 在 备份系统 之内
            Path::new("D:\\lib\\sys"),
            None,
        );
        assert!(r.is_err(), "已备份 在 备份系统 内 应拒绝");
    }

    // ── review-r3:两个不同 UNC 共享应触发跨分区警告(旧实现 qualifier 对 UNC 恒空 → 漏报)──
    #[test]
    fn check_paths_warns_on_different_unc_shares() {
        let w = check_paths(
            Path::new(r"\\srvA\share1\ready"),
            Path::new(r"\\srvB\share2\archived"),
            Path::new(r"\\srvB\share2\sys"),
            None,
        )
        .unwrap();
        assert!(
            w.iter().any(|s| s.contains("不在同一分区")),
            "不同 UNC 共享应有跨分区提醒,实际:{:?}",
            w
        );
    }

    // ── review-r3 round2:eq_ci 须用 Unicode 折叠(与 is_inside 一致),否则仅非 ASCII 大小写
    // 不同的同一目录(NTFS 视为同一)会绕过「同目录」安全闸 ──
    #[test]
    fn check_paths_rejects_same_dir_differing_only_in_non_ascii_case() {
        // Z: 几乎不存在 → canon 退化为词法路径、保留原大小写,正好考验 eq_ci 的折叠策略。
        let r = check_paths(
            Path::new(r"Z:\nope\Été"),
            Path::new(r"Z:\nope\été"),
            Path::new(r"Z:\nope\sys"),
            None,
        );
        assert!(
            r.is_err(),
            "仅非 ASCII 大小写不同的同一目录应被同目录闸拒绝(eq_ci 需 Unicode 折叠)"
        );
    }

    // ── review-r2 R2-6:folder_stable 的 minutes*60 不得溢出 panic(stable_minutes 用户可控)──
    #[test]
    fn folder_stable_huge_minutes_does_not_overflow() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f"), b"x").unwrap();
        // 极大稳定期:旧实现 minutes*60 在 debug 下 panic;修复后 cutoff 退化到 epoch → 文件比 cutoff 新 → 未稳定。
        let r = folder_stable(d.path(), u64::MAX);
        assert!(!r.stable, "极大稳定期应保守判未稳定,而非 panic");
    }
}
