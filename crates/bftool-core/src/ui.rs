//! 终端输出辅助：日志行打印 + 进度条创建。
//!
//! 保持极简：不引彩色库（在某些 Windows 环境会乱码或不显示），只用 emoji 前缀做轻量级强调。
//! indicatif 自带的进度条在 Windows 终端表现良好。

use indicatif::{ProgressBar, ProgressStyle};

pub fn info(msg: impl AsRef<str>) {
    println!("[i] {}", msg.as_ref());
}

pub fn ok(msg: impl AsRef<str>) {
    println!("[✓] {}", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    eprintln!("[!] {}", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    eprintln!("[x] {}", msg.as_ref());
}

pub fn action(msg: impl AsRef<str>) {
    println!("[>] {}", msg.as_ref());
}

/// 按字节量创建带速率/ETA 的进度条
pub fn bytes_bar(total: u64, label: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{prefix:>10} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    pb.set_prefix(label.to_string());
    pb
}

/// 按文件数创建简单进度条（archive 子命令暂未使用；verify / 未来批量校验时会用）
#[allow(dead_code)]
pub fn items_bar(total: u64, label: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("{prefix:>10} [{bar:30.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("=> "),
    );
    pb.set_prefix(label.to_string());
    pb
}
