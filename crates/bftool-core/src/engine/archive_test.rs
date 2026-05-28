//! 压缩包内部结构测试:调子进程跑 t 命令验证压缩包没坏。
//! 与 SHA256 字节级校验互补:SHA256 保证"字节没变",本模块保证"语义结构没坏"。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::engine::cruft;
use crate::reporter::Reporter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tester {
    WinRAR, // 优先:返回码最明确
    Bandizip,
    SevenZip,
}

#[derive(Debug, Clone)]
pub struct TesterPaths {
    pub winrar: PathBuf,
    pub bandizip: PathBuf,
    pub seven_zip: PathBuf,
}

/// 按 D3 顺序探测;找不到返回 None。
pub fn detect(paths: &TesterPaths) -> Option<(Tester, PathBuf)> {
    if paths.winrar.is_file() {
        return Some((Tester::WinRAR, paths.winrar.clone()));
    }
    if paths.bandizip.is_file() {
        return Some((Tester::Bandizip, paths.bandizip.clone()));
    }
    if paths.seven_zip.is_file() {
        return Some((Tester::SevenZip, paths.seven_zip.clone()));
    }
    None
}

/// 文件扩展名是不是**可测的压缩包主入口**(沿用 PowerShell 旧版列表)。
pub fn is_archive(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("zip" | "7z" | "rar" | "001" | "tar" | "gz" | "bz2" | "xz" | "cab" | "zipx")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultipartKind {
    Numeric,
    Zip,
    Rar,
}

fn multipart_kind(p: &Path) -> Option<MultipartKind> {
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())?;
    if !ext.is_empty() && ext.chars().all(|c| c.is_ascii_digit()) && ext.len() >= 3 {
        if let Ok(n) = ext.parse::<u32>() {
            if n >= 2 {
                return Some(MultipartKind::Numeric);
            }
        }
    }
    if let Some(rest) = ext.strip_prefix('z') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(MultipartKind::Zip);
        }
    }
    if let Some(rest) = ext.strip_prefix('r') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(MultipartKind::Rar);
        }
    }
    None
}

/// 分卷续卷的类型识别。
pub fn is_multipart_continuation(p: &Path) -> bool {
    multipart_kind(p).is_some()
}

/// 按 basename 精确配对返回应有主入口路径。
pub fn covering_main_for(p: &Path) -> Option<PathBuf> {
    let kind = multipart_kind(p)?;
    let parent = p.parent()?;
    let stem = p.file_stem().and_then(|s| s.to_str())?;
    let main_name = match kind {
        MultipartKind::Numeric => format!("{}.001", stem),
        MultipartKind::Zip => format!("{}.zip", stem),
        MultipartKind::Rar => format!("{}.rar", stem),
    };
    Some(parent.join(main_name))
}

fn tester_name(t: Tester) -> &'static str {
    match t {
        Tester::WinRAR => "WinRAR",
        Tester::Bandizip => "Bandizip",
        Tester::SevenZip => "7-Zip",
    }
}

#[derive(Debug, Default)]
pub struct TestReport {
    pub ok: bool,
    pub archive_failed: Vec<(PathBuf, String)>,
    pub tester_errors: Vec<String>,
    pub enum_errors: Vec<String>,
    pub archives_tested: usize,
    pub uncovered_files: usize,
}

impl TestReport {
    pub fn details(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (p, reason) in &self.archive_failed {
            out.push(format!("压缩包损坏：{} ({})", p.display(), reason));
        }
        for e in &self.tester_errors {
            out.push(format!("测试器执行异常：{}", e));
        }
        for e in &self.enum_errors {
            out.push(format!("枚举失败：{}", e));
        }
        out
    }

    pub fn summary(&self) -> String {
        let parts = self.details();
        if parts.is_empty() {
            "压缩包测试通过".to_string()
        } else {
            parts.join("; ")
        }
    }
}

enum InvokeOutcome {
    Ok,
    NonFatalWarn,
    ArchiveBad(i32),
    TesterError(String),
}

fn invoke_tester(tester: (Tester, &Path), archive: &Path) -> InvokeOutcome {
    let args: Vec<&str> = match tester.0 {
        Tester::WinRAR => vec!["t", "-ibck", "-y", "--"],
        Tester::Bandizip => vec!["t"],
        Tester::SevenZip => vec!["t", "--"],
    };
    let status_res = Command::new(tester.1)
        .args(&args)
        .arg(archive)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = match status_res {
        Ok(s) => s,
        Err(e) => {
            return InvokeOutcome::TesterError(format!(
                "spawn {} 失败 ({}): {}",
                tester_name(tester.0),
                tester.1.display(),
                e
            ));
        }
    };
    let Some(code) = status.code() else {
        return InvokeOutcome::TesterError(format!(
            "{} 进程异常退出(无 exit code,可能被信号杀掉或被 AV 拦截)",
            tester_name(tester.0)
        ));
    };
    match tester.0 {
        Tester::Bandizip if code == 0 => InvokeOutcome::Ok,
        Tester::Bandizip => InvokeOutcome::ArchiveBad(code),
        Tester::WinRAR | Tester::SevenZip if code == 0 => InvokeOutcome::Ok,
        Tester::WinRAR | Tester::SevenZip if code == 1 => InvokeOutcome::NonFatalWarn,
        Tester::WinRAR | Tester::SevenZip => InvokeOutcome::ArchiveBad(code),
    }
}

/// 测试一个文件夹下所有压缩包。
pub fn test_folder(folder: &Path, tester: (Tester, &Path), reporter: &dyn Reporter) -> TestReport {
    let mut report = TestReport::default();
    report.ok = true;
    let mut archives: Vec<PathBuf> = Vec::new();
    let mut continuations: Vec<PathBuf> = Vec::new();

    for entry in cruft::walk(folder) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let p = e.path().to_path_buf();
                if is_archive(&p) {
                    archives.push(p);
                } else if is_multipart_continuation(&p) {
                    continuations.push(p);
                } else {
                    report.uncovered_files += 1;
                }
            }
            Ok(_) => continue,
            Err(err) => {
                report.ok = false;
                report.enum_errors.push(format!("{}", err));
            }
        }
    }

    // 续卷配对(第十一/十二轮 P2):按 (parent, lower_filename) tuple key 精确配对
    let archive_keys: HashSet<(PathBuf, String)> = archives
        .iter()
        .filter_map(|p| {
            let parent = p.parent()?.to_path_buf();
            let name = p.file_name()?.to_str()?.to_ascii_lowercase();
            Some((parent, name))
        })
        .collect();
    for cont in &continuations {
        let covered = covering_main_for(cont).is_some_and(|main| {
            let Some(parent) = main.parent().map(|p| p.to_path_buf()) else {
                return false;
            };
            let Some(name) = main
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
            else {
                return false;
            };
            archive_keys.contains(&(parent, name))
        });
        if !covered {
            report.uncovered_files += 1;
        }
    }

    if archives.is_empty() && report.enum_errors.is_empty() {
        return report;
    }

    reporter.info(&format!(
        "测试 {} 个压缩包({})…",
        archives.len(),
        tester_name(tester.0)
    ));
    for a in &archives {
        report.archives_tested += 1;
        match invoke_tester(tester, a) {
            InvokeOutcome::Ok => {}
            InvokeOutcome::NonFatalWarn => {
                reporter.warn(&format!(
                    "压缩包有警告({} 返回码=1,非致命)：{}",
                    tester_name(tester.0),
                    a.display()
                ));
            }
            InvokeOutcome::ArchiveBad(code) => {
                report.ok = false;
                report.archive_failed.push((
                    a.clone(),
                    format!("{} 返回码={}", tester_name(tester.0), code),
                ));
                continue;
            }
            InvokeOutcome::TesterError(reason) => {
                report.ok = false;
                report.tester_errors.push(reason);
                break;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- is_archive ----
    #[test]
    fn is_archive_recognized_extensions() {
        assert!(is_archive(Path::new("a.zip")));
        assert!(is_archive(Path::new("a.7z")));
        assert!(is_archive(Path::new("a.rar")));
        assert!(is_archive(Path::new("a.001")));
        assert!(is_archive(Path::new("a.tar")));
        assert!(is_archive(Path::new("a.gz")));
        assert!(is_archive(Path::new("a.bz2")));
        assert!(is_archive(Path::new("a.xz")));
        assert!(is_archive(Path::new("a.cab")));
        assert!(is_archive(Path::new("a.zipx")));
    }

    #[test]
    fn is_archive_case_insensitive() {
        assert!(is_archive(Path::new("a.ZIP")));
        assert!(is_archive(Path::new("a.7Z")));
    }

    #[test]
    fn is_archive_with_path() {
        assert!(is_archive(Path::new("/some/dir/a.zip")));
    }

    #[test]
    fn is_archive_other_extensions_not_archive() {
        assert!(!is_archive(Path::new("a.jpg")));
        assert!(!is_archive(Path::new("a.mp4")));
        assert!(!is_archive(Path::new("a.002")));
        assert!(!is_archive(Path::new("a.z01")));
    }

    // ---- is_multipart_continuation ----
    #[test]
    fn continuation_numeric() {
        assert!(is_multipart_continuation(Path::new("foo.002")));
        assert!(is_multipart_continuation(Path::new("foo.003")));
        assert!(is_multipart_continuation(Path::new("foo.999")));
    }

    #[test]
    fn continuation_numeric_001_is_main_not_continuation() {
        assert!(!is_multipart_continuation(Path::new("foo.001")));
    }

    #[test]
    fn continuation_numeric_short_extension_not_continuation() {
        assert!(!is_multipart_continuation(Path::new("foo.2")));
        assert!(!is_multipart_continuation(Path::new("foo.99")));
    }

    #[test]
    fn continuation_zip_variants() {
        assert!(is_multipart_continuation(Path::new("foo.z01")));
        assert!(is_multipart_continuation(Path::new("foo.z99")));
    }

    #[test]
    fn continuation_zip_main_not_continuation() {
        assert!(!is_multipart_continuation(Path::new("foo.zip")));
    }

    #[test]
    fn continuation_rar_variants() {
        assert!(is_multipart_continuation(Path::new("foo.r00")));
        assert!(is_multipart_continuation(Path::new("foo.r01")));
    }

    #[test]
    fn continuation_rar_main_not_continuation() {
        assert!(!is_multipart_continuation(Path::new("foo.rar")));
    }

    #[test]
    fn continuation_normal_files_not_continuation() {
        assert!(!is_multipart_continuation(Path::new("foo.jpg")));
        assert!(!is_multipart_continuation(Path::new("foo.mp4")));
    }

    // ---- covering_main_for ----
    #[test]
    fn covering_main_numeric() {
        assert_eq!(
            covering_main_for(Path::new("path/movie.7z.002")),
            Some(PathBuf::from("path/movie.7z.001"))
        );
        assert_eq!(
            covering_main_for(Path::new("path/movie.7z.003")),
            Some(PathBuf::from("path/movie.7z.001"))
        );
    }

    #[test]
    fn covering_main_zip() {
        assert_eq!(
            covering_main_for(Path::new("path/split.z01")),
            Some(PathBuf::from("path/split.zip"))
        );
    }

    #[test]
    fn covering_main_rar() {
        assert_eq!(
            covering_main_for(Path::new("path/data.r00")),
            Some(PathBuf::from("path/data.rar"))
        );
    }

    #[test]
    fn covering_main_001_is_main_returns_none() {
        assert_eq!(covering_main_for(Path::new("path/movie.7z.001")), None);
    }

    #[test]
    fn covering_main_non_continuation_returns_none() {
        assert_eq!(covering_main_for(Path::new("path/foo.jpg")), None);
    }

    // ---- detect ----
    #[test]
    fn detect_returns_none_when_no_tester_installed() {
        let paths = TesterPaths {
            winrar: PathBuf::from("/nonexistent/winrar.exe"),
            bandizip: PathBuf::from("/nonexistent/bz.exe"),
            seven_zip: PathBuf::from("/nonexistent/7z.exe"),
        };
        assert!(detect(&paths).is_none());
    }
}
