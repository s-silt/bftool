//! bftool —— 归档备份工具
//!
//! 把 SSD 上「待备份」目录下的项目，以文件夹为单位整体归档到机械硬盘，
//! 跑 SHA256 三重校验、写本盘索引和全局索引、不删源（只移动）、可断点续传。
//!
//! 设计目标见 docs/design.md；CLI 子命令见 `bftool --help`。

use anyhow::Result;
use clap::Parser;

mod cli;
mod config;
mod engine;
mod ui;

fn main() -> Result<()> {
    // Windows 控制台默认是 GBK；把 stdout/stderr 切到 UTF-8，否则中文会乱码
    #[cfg(windows)]
    enable_utf8_console();

    let args = cli::Cli::parse();
    cli::dispatch(args)
}

#[cfg(windows)]
fn enable_utf8_console() {
    // 失败也不致命：用户路径如果没中文，GBK 控制台也能跑
    unsafe {
        // 65001 = CP_UTF8
        let _ = SetConsoleOutputCP(65001);
        let _ = SetConsoleCP(65001);
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn SetConsoleOutputCP(wCodePageID: u32) -> i32;
    fn SetConsoleCP(wCodePageID: u32) -> i32;
}
