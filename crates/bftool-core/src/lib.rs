//! bftool-core —— 归档备份工具核心引擎。
//!
//! 本 crate 提供与界面无关的纯业务逻辑：
//! - [`config`]：配置加载（TOML + 默认）
//! - [`domain`] / [`channel`] / [`pool`]：盘与介质抽象（P1）
//! - [`pipeline`]：操作阶段 + archive 切片（P1）
//! - [`service`]：统一 `run(OperationRequest)`（P1）
//! - [`engine`]：既有引擎（facade / 尚未迁完的模块）
//! - [`observe`]：结构化事件（hint/deny）
//! - [`reporter`]：与界面解耦的输出接口（CLI / GUI 各自实现）
//!
//! `bftool-core` 自身**不**做任何 `println!`，所有面向用户的输出都通过
//! [`reporter::Reporter`]。调用方（bftool-cli、bftool-gui）传一个 reporter
//! 实例即可。

pub mod channel;
pub mod config;
pub mod domain;
pub mod engine;
pub mod observe;
pub mod pipeline;
pub mod pool;
pub mod reporter;
pub mod service;
