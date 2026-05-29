//! 视图间共享的小工具:格式化 / 配置来源文案。(避免各视图重复)

use bftool_core::config::ConfigSource;

/// 字节 → 人类可读 GB（一位小数）。纯函数,可测。
pub fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
}

/// 配置来源 → 一句话文案(仪表盘 / 设置页共用)。
pub fn source_hint(src: &ConfigSource) -> String {
    match src {
        ConfigSource::Explicit(p) => format!("配置来源：命令行指定 {}", p.display()),
        ConfigSource::Candidate(p) => format!("配置来源：自动发现 {}", p.display()),
        ConfigSource::Default => "配置来源：内置默认（到「设置」保存一份固化你的配置）".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_gb_one_decimal() {
        assert_eq!(fmt_gb(0), "0.0 GB");
        assert_eq!(fmt_gb(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(fmt_gb(1024u64 * 1024 * 1024 * 3 / 2), "1.5 GB");
    }

    #[test]
    fn source_hint_mentions_origin() {
        use std::path::PathBuf;
        assert!(source_hint(&ConfigSource::Explicit(PathBuf::from("a.toml"))).contains("命令行"));
        assert!(
            source_hint(&ConfigSource::Candidate(PathBuf::from("b.toml"))).contains("自动发现")
        );
        assert!(source_hint(&ConfigSource::Default).contains("内置默认"));
    }
}
