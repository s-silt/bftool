//! 核心引擎：归档主流程 + 各子模块。
//!
//! 文件分工：
//! - status.rs   ── `bftool` 不带子命令时的状态总览
//! - drive.rs    ── 找盘 / 初始化 / 列出 / 序号管理
//! - manifest.rs ── 文件清单生成（Rel/Size/Hash/Mtime）+ 比对
//! - archive.rs  ── 主归档流程
//! - verify.rs   ── 复查（重算哈希、比对清单）
//! - find.rs     ── 全局索引查询
//! - safety.rs   ── 路径安全检查 / 稳定性检测
//! - txn.rs      ── 事务标记（进行中事务.txt）
//! - paths.rs    ── 盘内/系统路径常量与构造

pub mod archive;
pub mod drive;
pub mod find;
pub mod manifest;
pub mod paths;
pub mod safety;
pub mod status;
pub mod txn;
pub mod verify;
