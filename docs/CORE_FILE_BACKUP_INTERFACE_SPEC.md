# 普通目标目录备份：实际核心与 GUI 接口

独立候选树使用 pipeline::backup，用户选择文件/文件夹与普通目标目录即可计划、复制、校验和重试。无需初始化、格式化、登记磁盘、盘号或盘池。不依赖 Config.ready_root/archived_root/system_root；source 始终保留，禁止覆盖未知目标。

## 实际 API

类型由 bftool_core::pipeline::backup 导出。以下 service 函数都在后台线程调用：

```rust
service::plan_backup(&BackupRequest, &AtomicBool, &dyn Reporter) -> anyhow::Result<BackupPlan>;
service::run_backup_plan(&BackupPlan, &AtomicBool, &dyn Reporter) -> anyhow::Result<BackupSummary>;
service::verify_backup(&Path, &AtomicBool, &dyn Reporter) -> anyhow::Result<engine::verify::VerifyReport>;
service::list_backup_history(&Path) -> anyhow::Result<Vec<BackupHistoryRecord>>;
service::list_backup_history_with_cancel(&Path, &AtomicBool) -> anyhow::Result<Vec<BackupHistoryRecord>>;
service::resume_backup(&Path, job_id: &str, &AtomicBool, &dyn Reporter) -> anyhow::Result<BackupSummary>;
```

BackupRequest 实际字段为 source: SourceSelection::File(PathBuf) 或 Directory(PathBuf)、target_dir: PathBuf、conflict: ConflictPolicy::KeepBoth。
BackupPlan 执行绑定私有；view() 提供 job_id、destination_name、selected_target、entries、counts.files/directories、bytes、conflicts、issues。条目含 relative_path、File/Directory、bytes、sha256、identity、modified。request() 返回不可变的实际请求。

BackupSummary 有 outcome: Completed/Cancelled/Failed、published、copied、verified、skipped_verified、failed、bytes、issues。published 只在经过验证且持久 Completed 日志确认后为 true；false 也不能证明最终路径不存在。
执行错误保持 Result::Err；可 downcast 为 BackupExecutionError 读取 summary 和 source() 错误链。复制及校验进度可能只代表暂存内容；GUI 只把 published=true 批次计为完成备份，其他数据明确未发布/发布状态未确认。

## 版本与安全语义

首版仅 KeepBoth。已有同名项保留，本次整个所选文件/文件夹写到计划显示的新版本名，例如 folder (backup 1)。所有文件复制并做独立 SHA-256 校验，保留目录及空目录。大小相等不能认定内容一致，禁止凭 size/mtime 声称安全跳过。
发布采用 no-replace；晚到冲突、来源或目标身份/内容变化必须失败并重新计划。Reject、RenameConflict、SkipExisting 是原演示提议，不是当前实际枚举，界面说明这些策略未实现，不启用它们。

## GUI 线程与计划绑定

添加来源只保存路径，不在 UI 线程递归遍历、hash 或扫描全卷。后台计划分类、拒多源重叠/大小写别名，再逐源调用 core。多源计划 destination_name 冲突则拒绝整批，要求分批选择。
GUI 私有保存 Arc<Vec<BackupPlan>> 和独立输入签名；公开 FileBackupPlan 仅是显示模型，不得驱动执行。后台输入改变则结果丢弃；启动时必须仍匹配私有快照。多源串行执行，失败或取消后停后续批次，保留已确认发布的真实计数。

计划、执行、校验、历史读取统一 global busy。选择器、方法、拖放、演示预设、重复启动遵循门禁。TaskKind::Backup 独立于 Archive/Verify/Watch，备份取消不能取消别人的线程。关闭 App 时所有新线程请求取消并 join；取消请求等待实际结果，不能立即映射成功。
使用 GuiReporter 原有日志/字节进度描述实际阶段。Live 不提供伪造速率或 ETA；Demo 142 MB/s 明确标为纯合成示例。

## 普通目标目录校验、历史与恢复

Live 校验页直接选普通目标目录后台 verify_backup，顶层分支避开旧页面自动全卷扫描和初始化标记接口。Demo 保留纯内存示例。
任务页显式后台 list_backup_history 读取所选目录持久 manifest/journal，显示真实状态及错误；本次会话 last_summary 不当成持久历史。未完成 job 可选择“复验并按文件重试”，调用 resume_backup。恢复基于身份、manifest 和 hash 证据，是文件级重试，不宣称字节级断点。

## 空间和 Demo 隔离

仅后台查询所选目录空间；Windows GetDiskFreeSpaceExW，不枚举全卷。其他平台或查询失败明确未知，不将 0 当成已验证空间不足。SSD/HDD/Unknown 只是提示，不拒绝 Unknown 目标。
Demo 全部纯内存，禁真实选择器、目录读取、配置和盘扫描；预设在真实后台任务忙时不能改变任务。Live 切 Demo 只能空闲时完整加载纯内存 fixture，不可把执行中的 Live 结果改标为合成。

本规范取代原 engine::file_backup/FileBackupReporter 示例提议。copy-only 普通备份与旧盘池归档并行存在，不能借旧初始化/源移动语义实现此流程。
## 最终修复：主入口、结果归属与元数据协议

Live 主窗口仅提供四个主视图：备份、任务与记录、校验、设置。旧 Archive / Drives / Init / Watch / Find 视图即使由公开 view 字段设置，也在正式窗口分派前转回 Backup；旧磁盘管理入口只在空闲 Demo 中显示。旧配置设置明确标为兼容 CLI，不是普通文件备份的要求。默认入口与页脚始终使用普通目录 COPY；没有源移动或初始化的可达主流程。

Live 校验工作结果为 `ExecutionResult::PlainVerification { target: PathBuf, token: u64, result: Result<VerifyReport, String> }`。`VerifyUiState` 私有保存请求代数、活动目标及已校验目标。目标选择立即清除 report、stats、summary、error；每次 pump 和 Verify 绘制也比较公开目标字段，拒绝绕过选择器的旧结果。接受结果须同时匹配目标路径和令牌，成功与错误均带实际目标路径。旧无绑定 `Verification` 结果仅用于 Demo。Live 进度字节明确属于当前源；整批文件计数标为结束后更新。

新的 direct metadata 不调用旧 `write_atomic` / `write_atomic_new`。`SafeDir::write_metadata_new` 在 Windows 创建仅属于当前操作的独占临时对象，写入并 sync，在持有该对象期间执行 native no-replace rename。失败且尚未发布时只对该持有对象执行 disposition，不按历史临时文件名删除。成功返回实际对象身份供 checkpoint 复核。初始 `owner.json`、`manifest.json`、`journal.json` 和每个后续 checkpoint 全部走此边界。Linux 和其他不支持平台返回 Unsupported。

`journal.json` 是空的 Copying 初始记录，写后不改。后续为连续 `checkpoint-00000001.json` 等文件，每项是 `{ previous_sha256, change }`，包含前项原始字节的 SHA-256，以及一个带 kind 的 Directory / File / Publishing / Completed 增量。目录项只保存 path/identity，文件项只保存 path/receipt，发布项绑定 payload_id；Completed 只表示合法状态转换。读取检查：严格名称集合、连续编号、重复 JSON 键、字段及路径约束、完整哈希链、收据只增不改与状态合法迁移。执行期间保存每项对象身份与内容摘要。追加只复核最后一条记录、当前目录绑定以及新对象身份/字节，通过真实 no-replace 操作拒绝下一编号被占用。发布前（最后 Reporter 回调之后）和成功完成前独立全量复核所有记录身份/内容及精确名称集合；未知历史项从不覆盖。加载依序应用增量，拒绝重复收据与非法状态转换，最后通过索引对完整 manifest 校验一次。新的 checkpoint 名称已被占用时 fail closed；永不覆盖旧记录或未知对象。旧可变 journal 若非空初始状态则不自动采用，不提供迁移。

恢复仍是文件级：已记录 staging 文件须独立验证身份和 SHA-256 才可跳过；Publishing 且 stage 为空时核对完整可见 payload，之后追加 Completed。创建/发布 payload 和收据记录之间崩溃留下无收据对象时保留并停止；不自动删除孤儿。检查点临时残留、断号、未知文件、内容/身份变化均拒绝恢复。若发布后 checkpoint 失败，错误保留真实部分计数且 `published=false`，不宣称可见 payload 一定不存在。

增量格式以总路径和收据字节 B、记录数 n 计，元数据存储及固定次数全量审计读取为 O(B)，文件打开次数 O(n)，索引/名称排序校验为 O(n log n)。每追加不再 clone/validate 全部收据，也不重读历史前缀。每个目标的历史/校验只加载各任务一次，避免跨任务二次放大。元数据读取和摘要每 64 KiB 检查取消；链重构、名称枚举、任务加载、收据校验逐项检查。GUI 历史线程调用新可取消 facade；原 list_backup_history 保留为无取消的兼容包装。计划容量仅计 payload，metadata 和文件系统分配开销明确未知；0 字节 payload 不能作为空间充足保证。文件 sync 和 Windows rename 不提供断电/热拔插/恶意任意修改下的快照或耐久性保证。最终构建、Clippy、安全套件和针对修复快照的审查结果须另外记录；此规范不是发布批准。
