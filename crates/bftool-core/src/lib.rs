//! bftool-core —— 归档备份工具核心引擎。
//!
//! 本 crate 提供与界面无关的纯业务逻辑：
//! - [`config`]：配置加载（TOML + 默认）
//! - [`engine`]：稳定性检测、清单生成、三重校验、事务式提交、盘检测/初始化、复查、查询
//! - [`reporter`]：与界面解耦的输出接口（CLI / GUI 各自实现）
//!
//! `bftool-core` 自身**不**做任何 `println!`，所有面向用户的输出都通过
//! [`reporter::Reporter`]。调用方（bftool-cli、未来的 bftool-gui）传一个 reporter
//! 实例即可。

pub mod config;
pub mod engine;
pub mod reporter;
