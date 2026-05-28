//! bftool-core —— 归档备份工具核心引擎。
//!
//! 本 crate 提供与界面无关的纯业务逻辑：
//! - `config`：配置加载（TOML + 默认）
//! - `engine`：稳定性检测、清单生成、三重校验、事务式提交、盘检测/初始化、复查、查询
//! - `ui`：终端输出辅助（目前仍依赖 `println!`，将在下一批改造为 `BackupEvent` 事件流）
//!
//! 调用方（bftool-cli、未来的 bftool-gui）都通过此 crate 暴露的 API 工作。

pub mod config;
pub mod engine;
pub mod ui;
