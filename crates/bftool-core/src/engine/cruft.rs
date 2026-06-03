//! OS 注入的杂文件 / 目录,默认从 manifest、复制、容量统计、verify 全部排除。
//! 这些不是项目内容,无须用户配置。

use std::path::Path;
use walkdir::{DirEntry, WalkDir};

use crate::reporter::Reporter;

/// 文件名精确匹配（不分大小写）。
///
/// **不变量:这些条目必须全为 ASCII。** 匹配走 [`str::eq_ignore_ascii_case`](is_cruft_file),
/// 它只对 ASCII 字母做大小写折叠;若加入含非 ASCII 字符的名字,大小写不敏感会失效
/// (非 ASCII 字符按字节原样比较),导致漏判。OS 注入的杂文件名本身就都是 ASCII,无需突破此约束。
pub const CRUFT_FILES: &[&str] = &[
    "Thumbs.db",   // Windows 缩略图缓存
    "desktop.ini", // Windows 文件夹元数据
    ".DS_Store",   // macOS Finder 元数据
    "ehthumbs.db", // Windows Media Center 缩略图
    "ehthumbs_vista.db",
];

/// 目录名精确匹配（不分大小写）—— 整个目录跳过,不递归进去。
///
/// **不变量:这些条目必须全为 ASCII**(同 [`CRUFT_FILES`]:匹配用 `eq_ignore_ascii_case`,
/// 只折叠 ASCII 大小写)。系统注入的目录名($RECYCLE.BIN / System Volume Information 等)本就是 ASCII。
pub const CRUFT_DIRS: &[&str] = &[
    "$RECYCLE.BIN",
    "System Volume Information",
    "found.000",
    "lost+found",
    ".Trashes",
    "Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
];

/// 前缀模式：以 "._" 开头（macOS resource fork on non-HFS volumes）
pub fn is_cruft_file(name: &str) -> bool {
    if name.starts_with("._") {
        return true;
    }
    // bftool 原子复制的临时文件:不算项目内容,manifest/复制/校验/容量统计都忽略。(ledger L-014)
    if name.ends_with(".bftool-part") {
        return true;
    }
    CRUFT_FILES.iter().any(|c| c.eq_ignore_ascii_case(name))
}

pub fn is_cruft_dir(name: &str) -> bool {
    CRUFT_DIRS.iter().any(|c| c.eq_ignore_ascii_case(name))
}

/// 从 manifest 的 `Rel` 字段提取叶子名。
///
/// manifest 的 `Rel` 是 PowerShell 旧版兼容格式,**永远用 `\` 分隔**(哪怕在 Linux 跑测试)。
/// 直接用 `Path::new(rel).file_name()` 在非 Windows 平台会把整串 "foo\Thumbs.db" 当成叶子名,
/// cruft 判断漏判。这里手动按 `\` 和 `/` 都切,取最后一段。
pub fn leaf_name_from_rel(rel: &str) -> &str {
    rel.rsplit(['\\', '/']).next().unwrap_or(rel)
}

/// 判断 manifest `Rel` 是否包含 cruft。
///
/// 旧 manifest 可能登记 cruft 目录下的文件,例如:
/// - `$RECYCLE.BIN\foo.dat`(Windows 回收站子项)
/// - `.Trashes\1000\bar`(macOS 移动硬盘垃圾)
/// - `.fseventsd\checkpoint`(macOS FSEvents 状态)
///
/// 按 `\` 和 `/` 切**全部**路径段,任何一段命中 `is_cruft_dir` 或者叶子段命中
/// `is_cruft_file` → 整条 rel 视为 cruft。
pub fn rel_has_cruft_component(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return false;
    }
    // 中间段（任一目录段）命中 cruft 目录 → 是
    for seg in &parts[..parts.len() - 1] {
        if is_cruft_dir(seg) {
            return true;
        }
    }
    // 叶子段：文件名命中 → 是
    is_cruft_file(parts[parts.len() - 1])
}

/// 共享的 cruft-aware 遍历器。
///
/// 调用方拿到的 `DirEntry` 序列已经过滤掉:
/// - cruft 目录（递归整段跳过；root 自身豁免,depth==0 不当 cruft）
/// - cruft 文件名（精确 + `._` 前缀）
/// - 不跟 symlink / junction（follow_links=false）
///
/// **不**屏蔽 walkdir 自身的错误 —— 错误透传,由调用方决定怎么处理
/// (manifest::real_files: 收集后 bail；archive_test::test_folder: 同；
/// archive::copy_folder: 收集后 bail；verify: 计入 bad)。
pub fn walk(root: &Path) -> impl Iterator<Item = walkdir::Result<DirEntry>> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // depth==0 是 root 自身,不当 cruft
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() {
                !is_cruft_dir(&name)
            } else {
                !is_cruft_file(&name)
            }
        })
}

/// 扫描 `root`,对「按系统杂文件名单(CRUFT_DIRS / CRUFT_FILES)被排除、但其实含真实内容」的
/// 条目发可见 `warn`,把静默排除变可见 —— 防止用户真实数据(如恢复产物目录 `found.000`、或被
/// 命名成 `Thumbs.db` 的业务文件)被无声漏备。
///
/// **不**改变 [`walk`] 的对称过滤本身(那是 manifest/复制/verify 共用、必须对称,否则 verify 会
/// 误报 extra/missing);本函数只做一次只读侦测 + 告警。只针对**具名** cruft,刻意不含 `._` 前缀与
/// `.bftool-part`(那是真·临时/系统产物,告警反成噪音)。为防刷屏,最多提示前 `MAX_WARN` 条。(review-r3 round2)
pub fn warn_excluded_real_content(root: &Path, reporter: &dyn Reporter) {
    const MAX_WARN: usize = 20;
    let mut warned = 0usize;
    for entry in WalkDir::new(root).follow_links(false) {
        if warned >= MAX_WARN {
            break;
        }
        // WalkDir 枚举错误(权限/网络盘瞬断/路径过长)不静默跳过 —— 不可读子树里若有被排除的真实数据
        // 就漏报了,这正是本安全网最该报警的时刻。fail-loud:warn 一次而非裸 continue。(review-r3 round3)
        let e = match entry {
            Ok(e) => e,
            Err(err) => {
                reporter.warn(&format!(
                    "侦测被排除内容时枚举失败(可能漏报被静默排除的真实数据):{}",
                    err
                ));
                warned += 1;
                continue;
            }
        };
        if e.depth() == 0 {
            continue;
        }
        let path = e.path();
        let name = e.file_name().to_string_lossy();
        // 已位于某个 cruft 目录之内(父链含 cruft 目录段)→ 由该目录那条提示覆盖,不重复告警。
        let inside_cruft_dir = path
            .strip_prefix(root)
            .ok()
            .map(|rel| {
                let segs: Vec<String> = rel
                    .to_string_lossy()
                    .split(['\\', '/'])
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                segs.len() > 1 && segs[..segs.len() - 1].iter().any(|s| is_cruft_dir(s))
            })
            .unwrap_or(false);
        if inside_cruft_dir {
            continue;
        }
        if e.file_type().is_dir() {
            // 只对**含真实内容**的具名 cruft 目录告警。仅含 cruft 子项(如 found.000 里只有 Thumbs.db)
            // 不算真实数据,否则误报假阳告警。判据:存在「非 cruft 文件」或「非 cruft 子目录」即视为有真实内容。
            // read_dir / 子项类型读失败时 fail-loud:保守判有、照常告警(安全网宁多报不沉默)。(review-r3 round5)
            if is_cruft_dir(&name) {
                let has_real = match std::fs::read_dir(path) {
                    Ok(mut rd) => rd.any(|child| match child {
                        Ok(c) => {
                            let cn = c.file_name();
                            let cn = cn.to_string_lossy();
                            match c.file_type() {
                                Ok(ft) if ft.is_dir() => !is_cruft_dir(&cn),
                                Ok(_) => !is_cruft_file(&cn),
                                Err(_) => true, // 子项类型读不出 → 保守判有
                            }
                        }
                        Err(_) => true, // 子项枚举出错 → 保守判有
                    }),
                    Err(_) => true, // read_dir 失败 → 保守判有
                };
                if has_real {
                    reporter.warn(&format!(
                        "已按系统杂文件名单跳过目录「{}」及其内容(未纳入备份/校验)。若这是你的真实数据,请改名后重跑:{}",
                        name,
                        path.display()
                    ));
                    warned += 1;
                }
            }
        } else if e.file_type().is_file()
            && CRUFT_FILES.iter().any(|c| c.eq_ignore_ascii_case(&name))
        {
            // 只对具名 cruft 文件(Thumbs.db 等)且**非零字节**告警;`._`/`.bftool-part` 不在此列。
            // 读不出大小时 fail-loud:按「可能含内容」处理并告警,而非静默当空。(review-r3 round3)
            let non_empty = match e.metadata() {
                Ok(m) => m.len() > 0,
                Err(_) => true,
            };
            if non_empty {
                reporter.warn(&format!(
                    "已按系统杂文件名单跳过文件「{}」(未纳入备份/校验)。若这是你的真实数据,请改名后重跑:{}",
                    name,
                    path.display()
                ));
                warned += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ASCII 不变量(SEC-009)----
    #[test]
    fn cruft_constants_are_all_ascii() {
        // eq_ignore_ascii_case 只折叠 ASCII 大小写;非 ASCII 条目会让大小写不敏感失效。
        for c in CRUFT_FILES {
            assert!(c.is_ascii(), "CRUFT_FILES 含非 ASCII 条目：{c:?}");
        }
        for c in CRUFT_DIRS {
            assert!(c.is_ascii(), "CRUFT_DIRS 含非 ASCII 条目：{c:?}");
        }
    }

    // ---- is_cruft_file ----
    #[test]
    fn is_cruft_file_exact_names() {
        assert!(is_cruft_file("Thumbs.db"));
        assert!(is_cruft_file("desktop.ini"));
        assert!(is_cruft_file(".DS_Store"));
        assert!(is_cruft_file("ehthumbs.db"));
    }

    #[test]
    fn is_cruft_file_case_insensitive() {
        assert!(is_cruft_file("THUMBS.DB"));
        assert!(is_cruft_file("thumbs.db"));
        assert!(is_cruft_file("Desktop.INI"));
    }

    #[test]
    fn is_cruft_file_underscore_prefix() {
        assert!(is_cruft_file("._foo"));
        assert!(is_cruft_file("._image.jpg"));
        assert!(is_cruft_file("._"));
    }

    #[test]
    fn is_cruft_file_normal_files_not_cruft() {
        assert!(!is_cruft_file("main.zip"));
        assert!(!is_cruft_file("cover.jpg"));
        assert!(!is_cruft_file("readme.md"));
    }

    #[test]
    fn is_cruft_file_bftool_part_temp() {
        assert!(is_cruft_file("a.txt.bftool-part"));
        assert!(is_cruft_file("movie.7z.bftool-part"));
        assert!(!is_cruft_file("a.txt"));
    }

    // ---- is_cruft_dir ----
    #[test]
    fn is_cruft_dir_exact_names() {
        assert!(is_cruft_dir("$RECYCLE.BIN"));
        assert!(is_cruft_dir("System Volume Information"));
        assert!(is_cruft_dir(".Trashes"));
        assert!(is_cruft_dir(".fseventsd"));
    }

    #[test]
    fn is_cruft_dir_case_insensitive() {
        assert!(is_cruft_dir("$recycle.bin"));
        assert!(is_cruft_dir("system volume information"));
    }

    #[test]
    fn is_cruft_dir_no_subpath_match() {
        // 只看叶子名,不接受 "$RECYCLE.BIN/sub"
        assert!(!is_cruft_dir("$RECYCLE.BIN/sub"));
        assert!(!is_cruft_dir("foo/Thumbs.db"));
    }

    // ---- leaf_name_from_rel ----
    #[test]
    fn leaf_name_from_rel_simple() {
        assert_eq!(leaf_name_from_rel("Thumbs.db"), "Thumbs.db");
    }

    #[test]
    fn leaf_name_from_rel_backslash() {
        assert_eq!(leaf_name_from_rel("foo\\Thumbs.db"), "Thumbs.db");
        assert_eq!(leaf_name_from_rel("a\\b\\Thumbs.db"), "Thumbs.db");
    }

    #[test]
    fn leaf_name_from_rel_forward_slash() {
        assert_eq!(leaf_name_from_rel("a/b/Thumbs.db"), "Thumbs.db");
    }

    #[test]
    fn leaf_name_from_rel_mixed_separators() {
        assert_eq!(leaf_name_from_rel("a/b\\Thumbs.db"), "Thumbs.db");
        assert_eq!(leaf_name_from_rel("a\\b/Thumbs.db"), "Thumbs.db");
    }

    #[test]
    fn leaf_name_from_rel_empty() {
        assert_eq!(leaf_name_from_rel(""), "");
    }

    // ---- rel_has_cruft_component ----
    #[test]
    fn rel_has_cruft_component_normal_file() {
        assert!(!rel_has_cruft_component("foo.txt"));
        assert!(!rel_has_cruft_component("normal/path/foo.txt"));
    }

    #[test]
    fn rel_has_cruft_component_leaf_is_cruft_file() {
        assert!(rel_has_cruft_component("Thumbs.db"));
        assert!(rel_has_cruft_component("sub\\Thumbs.db"));
        assert!(rel_has_cruft_component("a/b/.DS_Store"));
    }

    #[test]
    fn rel_has_cruft_component_middle_segment_is_cruft_dir() {
        assert!(rel_has_cruft_component("$RECYCLE.BIN\\foo.dat"));
        assert!(rel_has_cruft_component(".Trashes\\1000\\bar"));
        assert!(rel_has_cruft_component("a/.fseventsd/checkpoint"));
    }

    #[test]
    fn rel_has_cruft_component_empty() {
        assert!(!rel_has_cruft_component(""));
    }

    // ── review-r3 round2:被系统杂文件名单静默排除、但含真实内容的条目应发可见 warn ──
    struct RecReporter(std::sync::Mutex<Vec<String>>);
    impl Reporter for RecReporter {
        fn log(&self, _level: crate::reporter::LogLevel, _msg: &str) {}
        fn warn(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
        fn progress_bytes(
            &self,
            _label: &str,
            _total: u64,
        ) -> Box<dyn crate::reporter::ProgressHandle> {
            Box::new(NoopProg)
        }
    }
    struct NoopProg;
    impl crate::reporter::ProgressHandle for NoopProg {
        fn inc(&mut self, _delta: u64) {}
        fn finish(&mut self) {}
    }

    #[test]
    fn warn_excluded_flags_named_cruft_with_real_content() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // 非空的具名 cruft 目录(恢复产物场景)
        std::fs::create_dir_all(root.join("found.000")).unwrap();
        std::fs::write(root.join("found.000").join("recovered.bin"), b"data").unwrap();
        // 具名 cruft 文件(非零字节)
        std::fs::write(root.join("Thumbs.db"), b"x").unwrap();
        // 普通文件 + 真·临时产物:都不应告警
        std::fs::write(root.join("real.txt"), b"hi").unwrap();
        std::fs::write(root.join("a.bftool-part"), b"tmp").unwrap();

        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        warn_excluded_real_content(root, &rep);
        let joined = rep.0.lock().unwrap().join("\n");

        assert!(
            joined.contains("found.000"),
            "应提示被排除的非空 found.000:{joined}"
        );
        assert!(
            joined.contains("Thumbs.db"),
            "应提示被排除的 Thumbs.db:{joined}"
        );
        assert!(!joined.contains("real.txt"), "普通文件不应被提示:{joined}");
        assert!(
            !joined.contains("bftool-part"),
            ".bftool-part 是真·临时产物,不应提示:{joined}"
        );
        assert!(
            !joined.contains("recovered.bin"),
            "cruft 目录内的文件由目录那条覆盖,不单独提示:{joined}"
        );
    }

    // ── review-r3 round5:仅含 cruft 子项的具名 cruft 目录(如 found.000 里只有 Thumbs.db)
    // 不是真实数据,不应误报假阳告警 ──
    #[test]
    fn warn_excluded_skips_cruft_dir_with_only_cruft_children() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("found.000")).unwrap();
        std::fs::write(root.join("found.000").join("Thumbs.db"), b"x").unwrap();
        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        warn_excluded_real_content(root, &rep);
        let joined = rep.0.lock().unwrap().join("\n");
        assert!(
            !joined.contains("found.000"),
            "仅含 cruft 子项的目录不应误报为真实数据;实际:{joined}"
        );
    }
}
