# 归档备份工具 (bftool)

> SSD → 机械盘，按文件夹安全归档，默认 SHA256 内容校验；校验和提交通过后才移动源，可中断后重试。
> 桌面版 `bftool-gui.exe`（浅色界面，8 个视图）与命令行 `bftool.exe` 共用核心引擎。

[![build](https://github.com/s-silt/bftool/actions/workflows/build.yml/badge.svg)](https://github.com/s-silt/bftool/actions/workflows/build.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## 这是给谁用的

你在 SSD 上做视频、图片、文档或代码项目，完成后希望把整个项目复制到机械硬盘冷存；一块盘写满后封盘、换盘，半年到一年后复查，并保留源文件供自己决定是否清理。

bftool 提供项目归档以及保留源的增量／监视流程，不提供版本化快照、加密、云同步或内容去重。需要这些功能可了解 [restic](https://github.com/restic/restic)、[rustic](https://github.com/rustic-rs/rustic) 或 [conserve](https://github.com/sourcefrog/conserve)。

本 README 描述当前 `main` 源码。已有 [Releases](https://github.com/s-silt/bftool/releases) 按各自发布版本为准，可能尚未包含最新改动；需要当前版本请从源码构建。

## 快速上手（桌面版 GUI）

1. 获取或构建 `bftool-gui.exe`，启动后先进入「设置」。Windows MSVC 构建静态链接 CRT；中文显示依赖系统中文字体，界面需要可用的图形环境。
2. 选择三个互不相等、互不嵌套的根目录并保存：**待备份**放源项目，**已备份**接收成功归档后的源，**备份系统**存放全局索引、事务记录和日志。先核对界面显示的实际配置来源。
3. 在「初始化」选择目标盘。工具写入约定的项目／本盘信息目录，不格式化磁盘；非空盘可选择并提示确认。系统盘、资料库盘有防误操作检查，危险的强制绕过需要再次确认。不要把唯一一份资料所在的盘当作试验盘。
4. 在「备份」先点 **预览计划**，核对源、目标盘、项目和风险提示，再点 **正式备份**。预览只观察，不复制、不移动源、不更新索引、不恢复旧事务；正式执行仍会重新检查源身份、目标占用、盘身份、封盘状态和校验策略。
5. 完成后保留日志并复查。盘封满后离线收好，换目标盘继续；发现失败或待人工核对记录时先检查证据，勿自行删除事务和部分目标。

预览绑定启动时的配置和选项；配置变化后旧结果会丢弃，不能用旧计划直接执行。后台任务未结束时不能应用设置或开始第二个任务，切换页面不会创建新的任务。

**取消是请求，不是立即终止当前文件复制。** 归档在项目边界检查取消；监视停止后续轮询，并等待当前归档安全收尾。写任务关闭窗口时也会请求取消并等待线程完成，可能需要一段时间；只读任务可以脱离窗口完成。跨页取消只作用于所属任务，底部状态栏可取消当前任务。强杀进程或断电与正常取消不同。

## 八个视图

| 视图 | 作用 |
|---|---|
| 仪表盘 | 当前盘、待备份数、容量及最近复查摘要 |
| 备份 | 配置源及归档选项，预览、正式执行、进度、日志与取消 |
| 复查 | 整盘、单项目目录或单文件验证；呈现结构化报告和问题明细 |
| 查找 | 从本机及可选外部索引定位项目所属盘和路径 |
| 盘列表 | 已识别备份盘及其状态 |
| 初始化 | 选择目标盘，查看阻拦原因和确认提示 |
| 监视 | 选择目录、过滤扩展名、配置轮询周期，增量归档并保留源 |
| 设置 | 编辑根目录、容量余量、稳定时间、压缩工具和外部索引，校验后保存 |

工作区及表格提供滚动容器，窄窗下卡片、步骤和设置字段重新排列。小窗口首屏不保证显示全部内容。

正式模式的 GUI 直接调用 `core/service`，不通过 shell 启动 CLI；纯合成 Demo 使用独立的内存后台，操作结果不代表生产引擎执行。

## 归档、增量与恢复机制

- **归档**：源身份与完整相对路径绑定计划；完成复制及默认 SHA256 校验后，按事务阶段提交清单和索引，再以不覆盖方式移动源。目标已占用或所有权不明时保留原目标，重新选择唯一名称或要求重新规划，不拿存在的目录当作续传授权。
- **增量**：`--incremental --retain-source` 保留源。判断“未变”需要完整内容快照及可信历史副本，不能仅靠文件数、大小和 mtime。旧 hash 缺失、副本缺失、路径变化或内容变化时重新归档；规划不认证未经验证的内容。该严格检查可能比元数据快跳慢。
- **监视**：轮询指定目录，默认保留源，用单实例锁协调写入；每轮沿用归档预检和内容验证。它不提供文件系统快照或文件编辑锁。
- **恢复**：中断后再次正式运行会检查阶段化事务、源／目标身份和真实提交状态。证据不足的旧事务、未知目标或未完成状态会保留并提示人工核对；不保证任意旧版本残留都能自动续传，更不保证从任意字节位置继续。
- **复查**：核对索引、项目树、清单及内容。缺失、损坏、读取失败、不可验证、多余文件及仅大小验证分别报告；取消或零检查不宣称内容完好。CLI 真实归档失败会返回非零状态。

普通文件 API 无法冻结外部进程已经打开的可写句柄。归档前请停止编辑源；本工具检查可检测的身份／内容变化，不提供对任意并发改写的快照保证。

## 配置与命令行

以 [bftool.toml.example](bftool.toml.example) 为模板。配置查找顺序为 CLI 显式 `--config` → 当前目录 `bftool.toml` → 可执行文件目录 `bftool.toml` → `%APPDATA%\bftool\config.toml` → 内置默认。GUI 保存可选择实际文件、应用数据目录或当前目录；不要在核对之前依赖默认的资料库路径。

关键参数包括 `ready_root`、`archived_root`、`system_root`、`reserve_gb`、`stable_minutes`、`min_drive_gb`、`name_prefix`、`test_archives` 和 `extra_catalogs`。监视轮询由 `watch_poll_secs` 配置或命令参数控制。相对目录以配置文件所在目录为基准。

```powershell
# 以下路径和盘符为示例；先修改配置并确认目标。
.\bftool.exe --config .\bftool.toml archive --dry-run
.\bftool.exe --config .\bftool.toml archive
.\bftool.exe --config .\bftool.toml archive --from .\ready --incremental --retain-source
.\bftool.exe --config .\bftool.toml watch .\ready --poll-secs 60 --ext zip,7z --recursive
.\bftool.exe --config .\bftool.toml verify E
.\bftool.exe --help
```

核心归档不要求安装第三方压缩工具。启用压缩包测试时可使用配置的 WinRAR、Bandizip 或 7-Zip；找不到工具会提示并继续默认 SHA256。关闭 SHA256 是危险模式，必须显式确认，不能同时关闭压缩测试；增量／监视禁止跳过 SHA256。

## 源码构建与检查

需要 Rust/Cargo 和 Windows MSVC 工具链（Visual Studio Build Tools 的 C++ 工具与 Windows SDK）；GUI 还需要系统中文字体和图形环境。声明的依赖门槛为 CLI/core Rust 1.85、GUI 1.88，精确最低版本未逐版验证；本轮 Windows 验证使用 Rust/Cargo 1.95.0、`x86_64-pc-windows-msvc` 目标。

```powershell
git clone https://github.com/s-silt/bftool.git
cd bftool
cargo build --workspace --release --locked --target x86_64-pc-windows-msvc
.\target\x86_64-pc-windows-msvc\release\bftool-gui.exe
cargo run -p bftool-cli --locked -- --help
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

`cargo build` 默认只构建 CLI；GUI 用 `-p bftool-gui`，全套用 `--workspace`。依赖已缓存时可以加 `--offline`。复现限定安全测试需要 Python 3，运行 `python scripts/test_safe_windows.py`；它明确排除会扫描实际卷／读取盘符或需要符号链接权限的测试，不应冒称完整测试套件通过。普通全量测试命令为 `cargo test --workspace --all-targets --locked`，只在先审阅测试路径并具备合适隔离环境时运行。

核心分层为 `service`（入口和策略）、`pipeline`（预检／计划／执行）、`pipeline/archive`（复制、内容校验、索引、增量、恢复）、`channel/pool`（介质事实与目标选择）；CLI 与 GUI 提供交互和报告。

## 纯合成 Demo 与截图

```powershell
.\target\x86_64-pc-windows-msvc\release\bftool-gui.exe --demo
.\target\x86_64-pc-windows-msvc\release\bftool-gui.exe --demo-screenshot .\demo-wide-100 --demo-zoom 1.0
.\target\x86_64-pc-windows-msvc\release\bftool-gui.exe --demo-screenshot .\demo-narrow-150 --narrow --demo-zoom 1.5
```

Demo 在创建 App、加载配置之前选定，使用合成配置、盘和结果，不扫描真实备份卷、不读取生产历史、不保存生产配置，也不打开文件／目录选择器。截图命令显式写入指定输出目录，仍会读取正常系统字体资产。

截图模式自动记录八页；默认窗口 1200×820，`--narrow` 为 880×600。`--demo-zoom` 是应用缩放（范围0.5–2.0），**不是 Windows 系统 DPI**。输出文件使用独占创建；目录内已有同名文件、写入失败、超时或提前关闭都会返回非零，不覆盖既有图像。每次使用新的输出目录。

## 验证范围

2026-10-03 的 Windows 限定安全验收：**344 通过、0 失败、33 项安全排除**（core/CLI 279，GUI 62，vendored XML兼容测试3）。33项为32项可达实际卷扫描／盘符读取及1项需要 Windows 符号链接权限；Linux 专属 cfg 测试未计入这些数字。fmt、严格 workspace Clippy、debug/release 构建通过。这是当前发布候选的结果，不能与历史版本计数累加。

最终 debug 程序的 **宽／窄 × 应用缩放100%／125%／150% × 八页＝48张**自身截图已复看。egui 指针事件测试覆盖三种任务的重复启动、跨页取消、完成和关闭；它们不等同于 OS 原生鼠标输入。原生鼠标／滚动因窗口焦点请求被拒未验证，未绕过限制。系统 DPI、真实备份卷、断电、外部压缩程序执行及 Linux 上的最终组合仍未验证。详见 [安全修复与兼容边界](docs/REPAIR_ACCEPTANCE.md)。CI 状态以页面上具体提交对应的运行记录为准。

本轮发布同步修复新 Rust CI 的浮点类型检查，并更新 quick-xml、webbrowser、anyhow、event-listener、memmap2 与 wayland-scanner。为保留原有可访问性功能，`vendor/zbus_xml` 保持4.x API并采用安全版本的 XML 解析器；来源、许可证和兼容修改见 [patch说明](vendor/zbus_xml/PATCH.md)。剩余 `number_prefix`、`paste`、`ttf-parser` 上游停止维护告警需要后续依赖迁移。

## 安全要点（请务必看）

1. **单份不算真备份**。每项目默认在一块盘上一份；重要资料采用3-2-1策略，保留多份及异地副本。
2. **气隙防勒索**。封盘后拔下、离线收好。
3. **定期复查并实际恢复抽检**。建议每6–12个月复查；“复制完成”不等于已经验证的备份（参见 [rsure](https://github.com/d3zd3z/rsure)）。
4. **总索引留份**。`备份系统\备份索引名单.csv` 另存一份。
5. **写满即拔**。只连接本次需要的目标盘，操作前再次核对盘身份。
6. **源由你决定清理**。正常归档校验提交后把源移动到已备份目录；保留源／监视模式不移动源。工具不做删除同步；错误、未知占用和证据不足时保留并报告，不能替代独立备份。

命令行细节、危险开关、盘内文件和手动恢复方式见 [other.md](other.md)；文档中的目录与盘符仅为示例。

## License

[MIT](LICENSE)
