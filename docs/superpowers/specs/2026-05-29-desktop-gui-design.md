# Spec D：桌面版(bftool-gui)设计

* 日期：2026-05-29
* 状态：待批准（user-review pending）
* 类型：功能 spec（新增 GUI 前端 + 配套 core API 塑形）
* 关联：[Spec C 修复 campaign](./2026-05-29-review-fix-campaign-design.md)、[ledger](../2026-05-29-review-fix-campaign-ledger.md)（L-021/L-031 在此落地)
* 框架决策：**egui / eframe**(纯 Rust、单静态 exe、零运行时依赖、同进程直接 link core)——经用户确认。
* 范围决策：**全功能**——覆盖 CLI 的 6 个显式子命令 + 默认 status 页——经用户确认。

---

## 1. 目标

给 bftool 一个**新手友好**的 Windows 桌面版:看状态、初始化盘、备份(含 dry-run 预览 + 实时进度)、复查、查找、改配置,全程图形化,不碰命令行。延续项目"单 exe、下载即用、无外部 .dll 依赖"的承诺。

**非目标**:重写引擎(GUI 是 core 的对等前端,与 CLI 并列);移动端/跨平台美化;远程/云。

## 2. 架构总览

新增 workspace 成员 `crates/bftool-gui`(eframe 二进制),**直接 link `bftool-core`**(同进程、无 IPC、不 shell 调 `bftool.exe`)。CLI 与 GUI 是 core 的两个对等渲染前端。

```
bftool-core (引擎,79 测试)
   ├── bftool-cli  (clap + TerminalReporter)   现有
   └── bftool-gui  (eframe + GuiReporter)       新增
```

- 单窗口 + 左侧栏切换多视图(多功能工具标准形态)。
- 单静态 exe(eframe 默认 glow/wgpu 后端;CRT 静态链接,沿用 release profile)。

## 3. 长任务、进度与取消(egui 单 UI 线程)

archive/verify 是长阻塞操作,**绝不能在 UI 线程跑**。

- **后台线程**:点「备份/复查」→ `std::thread::spawn` 跑 `core::archive::run` / `verify::run`。
- **`GuiReporter` 实现 core `Reporter` trait**:`log(level,msg)` → 推进 `mpsc::Sender<UiEvent>`;`progress_bytes()` 返回的句柄 → 更新 `Arc<Mutex<ProgressState>>`。UI 每帧 `try_recv` drain + `ctx.request_repaint()` 保持刷新。这是 `Reporter` 抽象的既定用途。
- **取消(收严)**:
  - core 的 `run`/`verify::run` 新增参数 `cancel: &AtomicBool`(或 `&CancelToken`)。
  - **archive 仅在项目边界检查**(每个项目开始前、完成后)——不在文件复制中途取消(那需要半复制目标的清理/续传/事务态设计,超出 MVP)。已开始的项目跑完或安全跳过。
  - **verify 可细粒度检查**(manifest / 项目 / 文件 之间),因为它只读、不改数据。
  - 取消的结果是**独立的 `Cancelled` 态**(新增到 `ArchiveSummary`/`VerifyReport` 或 outcome),**不计入 failed / corruption**;UI 显示"已取消(N 个已完成,其余未处理)"。
- CLI 当前的"Ctrl+C 中断 + 下次续传"语义不变;GUI 取消是它的图形化对等(同样靠 txn 标记保证一致性)。

## 4. core API 改动(随桌面版一并落地 L-021 / L-031 + 新增)

GUI 直接调 core,需把"只为 CLI 打印"的查询改成**返回结构化数据**;**CLI 渲染层保留**(把结构渲染成文本),**core 提供结构化查询 API,GUI 绝不解析 reporter 文本**。

### 4.1 结构化查询 + 计划(L-031 + 新增)
列表/明细/计划一律由 core 返回结构化数据,CLI 渲染成文本、GUI 直接消费,双方都不解析 reporter 文本。
- `find::search(cfg, keyword) -> Result<Vec<FindMatch>>`(`FindMatch { folder, drive_id, in_drive_path }`)。(取代 `find::run` 的 `println!`;配 L-042 测试)
- `status::gather(cfg) -> Result<StatusReport>`(`StatusReport { current_drive: Option<BackupDrive>, pending_count, last_verify: Option<LastVerify>, drives: Vec<BackupDrive> }`)。`last_verify` 数据来源见 §4.4。(取代 status 的 `println!`)
- `drives::gather() -> Result<Vec<BackupDrive>>`(GUI 列盘;或直接 `scan_mounted()`)。
- **`archive::plan(cfg, opts) -> Result<ArchivePlan>`(新增,Finding #1)**:不动数据算出本轮计划 —— `ArchivePlan { drive: BackupDrive, items: Vec<PlanItem> }`,`PlanItem { name, est_bytes, action: PlanAction }`。**`PlanAction = Archive { dest_name } | RenameAndArchive { dest_name }(重名)| Skip(reason)(未稳定 / 0 文件 …)| SealAndStop(reason)`**(余量不足 → **封盘并停止本轮**,而非 skip 该项目继续——对应现有 archive.rs 行为,Finding #2)。**dry-run = 渲染 plan;GUI 备份页列项目用 plan;正式 archive 与 plan 共享同一 per-project 决策逻辑**,不重复实现。CLI dry-run 改为渲染 ArchivePlan。
- **`VerifyReport` 扩明细(上轮 #2 + 本轮 #3 项目上下文)**:在 `checked/bad/extra` 计数外加 `issues: Vec<VerifyIssue>` 与 `extras: Vec<ExtraFile>`,**每条带项目上下文**(verify 逐 manifest/项目复查,同一 `rel` 在不同项目会重复):`VerifyIssue { project, rel, kind: Missing | SizeMismatch | Corrupt | Unverifiable | EnumError }`、`ExtraFile { project, rel }`。CLI 渲染成现有"· 项目名 / 逐行"文本;GUI 复查页能精确指向"哪个项目的哪个文件"。

### 4.2 类型化盘(L-021,分读写场景)
- `BackupDrive`:**已初始化的备份盘**通用类型(带 `sealed` 状态、容量、id、root)。`scan_mounted()` / `drives::gather()` 返回它——verify、drives、状态页都用它(需要读取所有已初始化盘,无论封盘与否)。
- `BackupDrive::try_into_writable(min_drive_gb: u64) -> Result<WritableDrive, DriveError>`(Finding #5):仅**未封盘且容量 ≥ min_drive_gb**时成功 —— 阈值**显式传入**(来自 `cfg.min_drive_gb`),不藏在无参方法里;**archive 写路径只接受 `WritableDrive`**,编译期挡掉"往封盘/过小盘写"。
- **init 候选(Finding #4)**:`drive::init_candidates(cfg) -> Result<Vec<InitCandidate>>`,`InitCandidate { letter, total_gb, is_system, is_library, non_empty, can_init: bool, block_reason: Option<String> }` —— 把现有 `init` 的防呆判定(系统盘/资料库盘/非空盘)抽成结构化,GUI 初始化页直接列、不重复实现防呆;`drive::init(letter, ..)` 执行签名不变。
- 不强迫所有调用吃 writable/sealed 二分:总类型 `BackupDrive` + 按需 `try_into_writable()`。

### 4.3 配置读写(LoadedConfig)
- `Config::load` → 返回 **`LoadedConfig { config: Config, source: ConfigSource }`**,`ConfigSource = Explicit(PathBuf) | Candidate(PathBuf) | Default`(记录"从哪读的")。
- **`LoadedConfig::save(target: SaveTarget) -> Result<()>`**:写哪由 `SaveTarget` 显式指定。**默认推荐 `%APPDATA%\bftool\config.toml`(与 cwd 无关,CLI 命令行运行与 GUI 双击启动都会查到)**——不拿"当前目录"作默认(Finding #6:GUI 双击的 cwd 与 CLI 在终端运行的 cwd 往往不同,写到 cwd 不保证 CLI 下次读到)。GUI 设置页:source 非 Default → 默认存回原处;Default → 默认 `%APPDATA%`,"当前目录"选项以**绝对路径**显示并注明"仅当 CLI 也在此目录运行才生效"。
- 写前过 `Config::validate()`(L-025)。CLI 的 `Config::load` 改基于 `LoadedConfig`(取 `.config`),行为不变。

### 4.4 last_verify 持久化(原 Finding #3;并解决 §3 "只读"冲突)
**verify 对备份盘保持严格只读**——不写盘内任何文件。否则会违反 §3 "verify 只读"前提、让封盘/写保护盘无法复查、并把"元数据写失败"与"数据损坏"混为一谈。
复查时间改记到**本地 `system_root`**:成功完成后写/更新 `备份系统\复查记录.csv`(列 `drive_id, last_verify_utc`)。`status::gather` 读它 → `last_verify: Option<LastVerify { drive_id, when }>`(无记录 = 未知/None)。这与 system_root 已承载全局索引/序号/状态一致;盘内仍自洽(校验清单在盘上,复查时间属本机操作记录)。**取消或失败的复查不更新该记录**。配测试(写+读往返、缺记录→None)。

### 4.5 取消钩子
- `archive::run(.., cancel: &AtomicBool)`、`verify::run(.., cancel: &AtomicBool)`(CLI 传一个永不取消的常量,行为不变;GUI 传可置位的)。

所有 core 改动各配回归测试(core 已有测试基建 + temp_world 集成脚手架)。

## 5. 视图(全 7,左侧栏切换)

| 视图 | 对应 CLI | 要点 |
|---|---|---|
| **仪表盘(默认)** | 默认 status | 当前盘 / 待备份数 / 上次复查;三大按钮「备份 / 复查 / 初始化新盘」。首次无配置或无盘 → 顶部横幅引导去设置/init。 |
| **备份** | `archive` | 列待备份项目(`archive::plan`)→「演练(= 渲染 plan)」+「正式备份」;实时进度条 + 日志 + 成功/失败/取消汇总。**高级设置(默认折叠)**:`--limit`、指定盘、`stable_minutes`、`reserve_gb`、`--no-test-archives`、`--unsafe-no-hash`(+ 确认)。**危险组合**(no_hash + 关测试)沿用 core `verify_disabled` fail-closed + GUI 二次确认弹窗。「取消」按钮(项目边界生效)。 |
| **复查** | `verify` | 选盘(`BackupDrive` 列表)→ 进度 + 损坏/缺失/多余报告(`VerifyReport`);可取消。 |
| **初始化** | `init` | `drive::init_candidates` 列原始盘(盘符/容量/防呆阻断原因)→ 选可初始化的 → init;`--force` 藏在二次确认后。 |
| **查找** | `find` | 关键词 → 结果表(`Vec<FindMatch>`:在哪块盘 / 盘内路径)。 |
| **盘列表** | `drives` | 列已识别 `BackupDrive`(可用/已封盘/容量)。 |
| **设置** | `config-show` | **配置预览**(当前生效 `LoadedConfig`:config + 来源路径)+ **编辑**三根目录与选项 → `validate` → `save`(默认 `%APPDATA%`,见 §4.3)。 |

新手友好:默认值保守;危险操作二次确认;错误信息带"怎么修"(沿用 core 文案);dry-run 预览先行。

## 6. 错误 / 取消 / 状态处理

- core 函数返回 `Result`;GUI 在状态区/弹窗显示错误(error 级 reporter 日志进日志面板)。
- 长任务三态:`Done(summary)` / `Failed(err)` / `Cancelled(partial)`,UI 分别呈现。
- 不吞错(沿用 campaign 规约);GUI 不掩盖 core 的 fail-closed。

## 7. 文件结构 & 测试

`crates/bftool-gui/src/`:
- `main.rs`(eframe 入口)、`app.rs`(`App` + `View` enum + update 循环)
- `views/`(`dashboard.rs` / `archive.rs` / `verify.rs` / `init.rs` / `find.rs` / `drives.rs` / `settings.rs`)
- `reporter.rs`(`GuiReporter` → channel/进度)、`task.rs`(后台线程 + 取消 + 进度状态机)

**测试**:`GuiReporter` 事件投递、`task` 状态机、视图启用逻辑(纯逻辑)用单测;eframe 渲染不强测。core 改动各配回归测试。CI `--workspace --all-targets` 自动纳入 gui crate。

## 8. 不做的事(YAGNI)

- 不在 archive 文件复制中途取消(见 §3)。
- 不做 restore 子命令 / 加密 / 去重 / 云(各属独立问题域)。
- 不追求跨平台像素级美化;Windows-first。
- L-011/L-012(事务恢复微秒崩溃窗口)仍按 campaign 取舍留 backlog,不在本 spec。

## 9. 与 campaign 的衔接

本 spec 的 §4 落地 campaign 取舍出去的 **L-021**(类型化盘,这里细化为 BackupDrive + try_into_writable)与 **L-031**(结构化查询,不 println 旁路),并新增 `LoadedConfig`/`save` 与取消钩子——都是"GUI 直接调用 core"自然需要的 API 形状。
