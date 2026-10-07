# bftool 文件备份

选择 SSD 上的文件或文件夹，复制到自己选择的普通目标目录。正式备份始终保留源文件，强制 SHA-256 校验；不要求初始化、格式化、登记磁盘或配置三个旧归档根目录。

本目录是 Windows 普通目录复制候选源码；发行资产仅提供 Windows x64 GUI。旧版本二进制及历史验收结果不能代表此候选。

## 使用 GUI

1. 启动 `bftool-gui.exe`，默认进入“备份”。添加文件或文件夹，选择普通目标目录。目标目录可以已有其他内容。
   每个新添加的文件夹默认只备份当前层普通文件，不进入子文件夹。需要递归时，明确勾选该来源的“包含子文件夹”。可启用后缀筛选并选择或输入多个后缀，例如 `tar, zip`，另有“无后缀文件”选项；清空筛选恢复当前范围内的所有文件。筛选大小写不敏感，`.JPG` 与 `jpg` 相同。按最后一段后缀判断，`a.tar.gz` 属于 `gz`，不属于 `tar`。显式添加的单个文件不受文件夹筛选影响。
2. 生成预览计划，核对源、目标、内容数量与名称冲突。预览只读；修改选择后必须重新生成计划。
3. 正式开始复制。重名默认保留两份，使用计划中可见的新名称；晚到冲突会停止并要求重新规划，不覆盖未知内容。
4. 完成后在“校验”中对所选目标目录重新校验，在“任务与记录”中读取持久化历史。只有确认发布并完成校验的批次才计为成功。
5. 未完成任务可复查后按文件重试；恢复先核对源、目标身份、清单、文件哈希及检查点。没有逐字节续传保证，未知残留会保留并拒绝自动采用。

主窗口提供备份、记录、校验和设置。Live 主窗口不开放旧的源移动归档、磁盘管理、初始化或监听执行页面。普通文件备份不扫描所有卷。设置页中的旧归档配置仅供兼容命令行工作流使用，不是文件备份的前置步骤。

选择、预览、执行、校验和历史读取遵循全局任务忙状态。修改后缀筛选、无后缀文件选项或递归范围会使旧预览失效，必须重新生成计划；规则随实际计划保存，执行与恢复不会扩大到未选文件。筛选后没有匹配文件会显示 0 项并阻止开始，不计为成功备份。取消会请求工作线程安全停止；窗口关闭时等待工作线程退出。Live 多源进度显示当前源的字节进度，最终整批成功数以已发布结果为准。改变目标会清除旧校验报告；结果只属于其实际校验目标和请求。

## 存储与恢复边界

正式开始时，在所选目标的 `.bftool-backup` 下自动建立归属元数据。初始 `journal.json` 保持不可变，后续使用连续编号的不可变增量检查点和前项 SHA-256 链，每条记录只保存一个目录/文件收据或状态转换。所有新元数据均通过持有的临时文件对象执行 no-replace 发布；失败清理只作用于该对象。未知命名、内容、身份、断链或临时残留均导致保留并拒绝恢复。旧的可覆盖 journal 格式不自动迁移。较早直接复制任务未记录后缀和递归选项时，恢复保留其原有“递归、全部文件”范围，不套用新任务的当前层默认值；历史记录显示任务原有规则。

文件同步和受保护发布并不构成任意断电、热拔插或外部并发编辑下的文件系统快照。发布完成与检查点落盘之间崩溃时，根据已有证据复验；证据不足则停止，源仍保留。增量格式的元数据空间、累计元数据读取字节和文件打开次数随条目数线性增长；恢复与最终校验使用 O(n log n) 索引验证，不再重复序列化或重读完整历史。普通备份的复制和文件 SHA-256 校验循环按最多 1 MiB 分块检查取消；journal 元数据读取按 64 KiB 分块检查，任务和目录枚举逐项检查。取消可在当前文件未完成时响应，该文件可能需要重新复制；不承诺等整文件完成或固定响应时间。预览容量仅含文件内容，元数据和文件系统分配开销未知；0 字节文件也需要空间，不能把 payload 数字当作足够容量保证。

## 遗留 CLI：不同语义

`bftool.exe archive`、`watch`、旧 `verify` 及磁盘池初始化是保留的兼容功能，不是上述 GUI 文件复制流程。旧归档依赖 `ready_root`、`archived_root`、`system_root` 和已登记备份盘；默认成功提交后会把源移到归档根目录。`--retain-source` 才选择保留源。请先读 [other.md](other.md)、[配置示例](bftool.toml.example) 与命令帮助，不能把旧 archive 命令当成普通目录 copy-only API。

```powershell
.\bftool.exe --help
.\bftool.exe archive --help
```

## 构建与 Demo

需要 Rust/Cargo 和 Windows MSVC C++ 工具链及 Windows SDK。GUI 需要图形环境和中文字体。构建使用工作区现有锁文件；依赖缓存齐全时可添加 `--offline`。

```powershell
cargo build --workspace --locked --target x86_64-pc-windows-msvc
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
.\target\x86_64-pc-windows-msvc\debug\bftool-gui.exe --demo
```

Demo 使用独立的纯内存后端；示例数据与速度不代表实际备份。截图模式仅向明确指定的输出目录写入。

## 验证状态

历史文档中的 344 项测试和 48 张截图属于较早版本，不能作为此候选通过的证据。每次发行须以对应提交的构建、格式、严格 Clippy、安全合成测试及审查证据为准，并同时报告安全排除和平台边界。

Windows 限定的安全测试脚本为 `scripts/test_safe_windows.py`，其排除项必须与结果一并报告，不能声称全量真实卷测试通过。此次不操作真实磁盘、系统设置或账户。真实硬盘/SSD、重解析/热插拔、断电、原生鼠标/DPI 仍需另行验证。新的持有对象复制、元数据和发布操作在 Linux 返回 Unsupported；不声称 Linux 可用或已测试。

核心 API 与 GUI 适配约定见 [接口规范](docs/CORE_FILE_BACKUP_INTERFACE_SPEC.md)。

[MIT License](LICENSE)


## Tauri desktop migration candidate

A first complete backup workflow is available in [apps/desktop](apps/desktop/README.md), with [architecture and validation scope](docs/UI_MIGRATION.md). It retains the Rust backup core and existing CLI/egui. The independent frontend/native/bridge locks and desktop CI are separate from the original workspace. This candidate is unsigned and requires WebView2 Runtime; it is not a stable release or a full graphics-compatibility acceptance.
