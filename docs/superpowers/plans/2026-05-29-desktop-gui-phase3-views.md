# 桌面版 Phase 3：剩余 5 视图 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: `superpowers:executing-plans`(inline)。Steps 用 `- [ ]`。每视图单 task 单 commit;每步后跑 §门禁。**cargo 用 PowerShell 跑**。沿用 Phase 2 既定模式(view 模块导出 `pub fn ui(app,ui)`,UI 状态结构体挂 `App`,长任务走 `BackgroundTask`+`GuiReporter`)。

**Goal:** 补齐左侧栏剩余 5 个视图——盘列表 / 查找 / 初始化 / 复查 / 设置——各消费 Phase 1 已就绪的结构化 core API,渲染层在 GUI。完成后 7 视图全可用,桌面版功能完整。

**Architecture:** 每视图一个 `views/<name>.rs`(导出 `pub fn ui(app: &mut App, ui: &mut egui::Ui)`),跨帧 UI 状态用挂在 `App` 的 `<Name>UiState`(同 Phase 2 `ArchiveUiState`)。**复查**是唯一长任务,复用 archive 的后台机制(`BackgroundTask`/`GuiReporter`/`progress`/`logs`/`rx`/cancel)。其余(drives/find/init/settings)同步跑(均为快操作),只读列表带「刷新」按钮 + 缓存避免每帧重扫。

**Tech Stack:** eframe/egui、bftool-core(verify/find/drive/config API)。

**门禁(每步)**:`cargo build -p bftool-gui` / `cargo test --workspace --all-targets` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check`,全绿。GUI 渲染靠编译 + 实跑;测试覆盖纯逻辑(格式化/状态映射/校验)。

**隔离**:`desktop-gui` 分支(已与 main 同步)。完成开 PR → main,squash 合并(我自管 git)。

**任务顺序**(简单→复杂):T1 盘列表 → T2 查找 → T3 初始化 → T4 复查(后台)→ T5 设置(表单 + save)。每 task 在 `app.rs` 的 `update()` match 加上该视图的实路由(替换 placeholder)。

---

## 已就绪的 core API(本阶段只消费,不改 core)

- `drive::scan_mounted() -> Result<Vec<DriveInfo>>`(`DriveInfo{letter,root,id,sealed,free_bytes,total_bytes}`)
- `find::search(cfg, keyword) -> Result<Vec<FindMatch>>`(`FindMatch{folder,drive_id,archived_time,project_no,in_drive_path,verify}`)
- `drive::init_candidates(cfg) -> Result<Vec<InitCandidate>>`(`InitCandidate{letter,total_gb,is_system,is_library,already_backup,non_empty,can_init,block_reason}`)
- `drive::init(cfg, reporter, letter:&str, id:Option<&str>, force:bool) -> Result<()>`
- `verify::run(cfg, reporter, drive_letter:Option<&str>, cancel:&AtomicBool) -> Result<VerifyReport>`(`VerifyReport{checked,bad,extra,issues:Vec<VerifyIssue>,extras:Vec<ExtraFile>,cancelled}` + `has_corruption()`/`outcome()`)
- `LoadedConfig::save(&SaveTarget) -> Result<PathBuf>`(先 `validate()`)、`SaveTarget = CurrentSource|AppData|CurrentDir|Custom`、`appdata_config_path()`、`Config::validate()`

---

### Task 1：盘列表视图(drives)

**Files:** Create `crates/bftool-gui/src/views/drives.rs`;Modify `views/mod.rs`、`app.rs`(加 `DrivesUiState`、路由、init)

只读列表 + 缓存 + 刷新(避免每帧 `scan_mounted` 扫盘)。

- [ ] **Step 1** `app.rs` 加字段 `pub drives_cache: Option<Vec<bftool_core::engine::drive::DriveInfo>>`(init `None`)。`views/mod.rs` 加 `pub mod drives;`。
- [ ] **Step 2** `drives.rs`:`ui(app,ui)`:标题 + 「刷新」按钮(点击 → `scan_mounted()` 存 `app.drives_cache`,出错进日志或就地 error label);首次进入若 cache 为 None 自动扫一次。渲染:每盘一行(`id (letter:) 剩余 fmt_gb/共 fmt_gb`,可用/已封盘),空 → 提示插盘 init。复用一个共享 `fmt_gb`(从 dashboard 抽到 `views/mod.rs` 或新 `views/util.rs` 公共函数,dashboard 改用它,去重)。
- [ ] **Step 3** `app.rs` update() 加 `View::Drives => crate::views::drives::ui(self, ui),`。
- [ ] **Step 4** 纯逻辑测试:`fmt_gb`(若移到 util,测试随之搬);`drives.rs` 若有 `drive_tag(sealed)->&str` 之类小函数配测。
- [ ] **Step 5** build + test + clippy + fmt 绿。
- [ ] **Step 6** commit：`feat(gui): 盘列表视图(scan_mounted + 刷新缓存) (Phase3 T1; Spec D §5)`

---

### Task 2：查找视图(find)

**Files:** Create `views/find.rs`;Modify `mod.rs`、`app.rs`(加 `FindUiState`)

- [ ] **Step 1** `app.rs` 加 `pub find_ui: crate::views::find::FindUiState`(init default)。`mod.rs` 加 `pub mod find;`。
- [ ] **Step 2** `find.rs`:`FindUiState{ keyword:String, results:Option<Vec<FindMatch>>, error:Option<String> }`(derive Default;`FindMatch` 来自 core)。`ui`:关键词单行输入 + 「查找」按钮(回车也触发);点击 → `find::search(&app.cfg,&kw)`,Ok 存 results、Err 存 error。渲染结果表(`egui::Grid`:文件夹名/备份盘/盘内路径/校验;空结果显式"无匹配")。
- [ ] **Step 3** `app.rs` update() 加 `View::Find => crate::views::find::ui(self, ui),`。
- [ ] **Step 4** 纯逻辑测试:`FindUiState::default()` 空;(可选)一个把 `FindMatch` 行渲染成 4 列文本的 helper `match_row(&FindMatch)->[String;4]` 配测。
- [ ] **Step 5** 门禁绿。
- [ ] **Step 6** commit：`feat(gui): 查找视图(find::search + 结果表) (Phase3 T2; Spec D §5)`

---

### Task 3：初始化视图(init)

**Files:** Create `views/init.rs`;Modify `mod.rs`、`app.rs`(加 `InitUiState`)

- [ ] **Step 1** `app.rs` 加 `pub init_ui: crate::views::init::InitUiState`。`mod.rs` 加 `pub mod init;`。
- [ ] **Step 2** `init.rs`:`InitUiState{ cache:Option<Vec<InitCandidate>>, selected:Option<String>/*letter*/, force:bool, confirm_force:bool, custom_id:String }`。`ui`:「刷新候选」→ `init_candidates(&app.cfg)` 存 cache(首次自动);渲染每个候选(`letter total_gb` + can_init ? 单选 : 灰显 block_reason);选中可初始化的 → 「初始化为下一个备份N」按钮(可填 custom_id);`force` 复选(藏在 `CollapsingHeader`"高级"内)+ 选了 force 必须再勾 `confirm_force` 才放行(危险操作二次确认)。点击 → `drive::init(&app.cfg, &reporter, &letter, custom_id_opt, force)`(同步,reporter 收集进日志);成功后清 cache 触发重扫 + 提示。
- [ ] **Step 3** `app.rs` update() 加 `View::Init => ...`。
- [ ] **Step 4** 纯逻辑测试:`candidate_label(&InitCandidate)->String`(can_init → "可初始化";否则含 block_reason)配测;force-gate 逻辑 `fn init_allowed(force,confirm)->bool`(force ⇒ 需 confirm)配测。
- [ ] **Step 5** 门禁绿。
- [ ] **Step 6** commit：`feat(gui): 初始化视图(init_candidates 防呆列表 + force 二次确认) (Phase3 T3; Spec D §5)`

---

### Task 4：复查视图(verify;唯一后台任务)

**Files:** Create `views/verify.rs`;Modify `mod.rs`、`app.rs`(加 `VerifyUiState` + 复用后台机制)

复查长阻塞 → **复用 archive 的后台机制**(`BackgroundTask`/`GuiReporter`/`progress`/`logs`/`rx`/cancel)。

- [ ] **Step 1** `app.rs` 加 `pub verify_ui: crate::views::verify::VerifyUiState`。`mod.rs` 加 `pub mod verify;`。
- [ ] **Step 2** `verify.rs`:`VerifyUiState{ selected:Option<String>/*letter,None=自动单盘*/, last_report:Option<VerifyReportSummary> }`(`VerifyReportSummary` 自定义轻量:checked/bad/extra/issues 文本列表 —— 因 `VerifyReport` 在后台线程产出,摘要经 task 的 String 回传 + issues 经 GuiReporter 日志;Phase 3 先靠日志面板展示逐项 issue,summary 行展示 checked/bad/extra,不必把 VerifyReport 整个搬过线程)。`ui`:盘选择(从 `scan_mounted` 列 BackupDrive 单选,或"自动")+「开始复查」按钮(`!busy && 选了盘或自动`)→ 同 archive:建 channel + GuiReporter,`app.task = BackgroundTask::spawn(move |cancel| { let r = verify::run(&cfg, &reporter, sel.as_deref(), cancel)?; Ok(format!("复查完成：检查 {} · 损坏/缺失 {} · 多余 {}{}", r.checked, r.bad, r.extra, if r.cancelled {" · 已取消"} else {""})) })`;进度条 + 日志面板(复用 app.progress/logs/rx,与 archive 同一套渲染——可抽 `views/util::progress_and_log(app,ui)` 公共片段,archive 也改用,去重)+ 取消按钮 + 摘要。
- [ ] **Step 3** `app.rs` update() 加 `View::Verify => ...`。
- [ ] **Step 4** 纯逻辑测试:`verify_summary(checked,bad,extra,cancelled)->String`(含关键数字/取消字样)配测。
- [ ] **Step 5** 门禁绿(`cargo test --workspace`)。
- [ ] **Step 6** commit：`feat(gui): 复查视图(后台 verify::run + 进度/日志/取消) (Phase3 T4; Spec D §3/§5)`

---

### Task 5：设置视图(settings;表单 + 校验 + 保存)

**Files:** Create `views/settings.rs`;Modify `mod.rs`、`app.rs`(加 `SettingsUiState`)

- [ ] **Step 1** `app.rs` 加 `pub settings_ui: crate::views::settings::SettingsUiState`。`mod.rs` 加 `pub mod settings;`。**进入设置视图时**若 `settings_ui` 未初始化 → 从 `app.cfg` 填充表单字段(惰性初始化标志 `loaded:bool`)。
- [ ] **Step 2** `settings.rs`:`SettingsUiState{ loaded:bool, ready_root:String, archived_root:String, system_root:String, reserve_gb:String, stable_minutes:String, min_drive_gb:String, name_prefix:String, test_archives:bool, winrar:String, bandizip:String, seven_zip:String, target:SaveChoice, last_result:Option<String> }`;`SaveChoice = CurrentSource|AppData|CurrentDir`(映射到 core `SaveTarget`;Custom 暂不在 GUI 暴露,避免文件选择器依赖)。`ui`:
  - 顶部显示**当前来源**(`app.config_source`,复用 dashboard 的 source 文案 helper —— 抽到 util)。
  - 表单:3 个根目录文本框 + reserve/stable/min 数字(文本框,解析失败提示)+ name_prefix + test_archives 复选 + 3 个测试器路径(折叠"高级")。
  - 保存位置单选(默认 AppData;CurrentSource 当 source==Default 时禁用并注明)。
  - 「保存」按钮 → 用表单构造 `Config`(数字解析失败 → 就地 error,不保存)→ `LoadedConfig{config, source: app.config_source.clone()}.save(&target)`;成功 → 显示写入路径 + 更新 `app.cfg`/`app.config_source`(若新建则 source 变 Explicit/Candidate 对应路径);`validate()` 失败(core)→ 显示其"怎么修"文案。
- [ ] **Step 3** `app.rs` update() 加 `View::Settings => ...`(并删除 `other => placeholder` 分支——此时 7 视图全部实路由)。
- [ ] **Step 4** 纯逻辑测试:`fn parse_form(&SettingsUiState)->Result<Config,String>`(数字解析 + 空根目录校验;解析失败给中文原因)配测(合法→Ok、非数字→Err 含字段名);`SaveChoice→SaveTarget` 映射配测。
- [ ] **Step 5** 门禁绿。
- [ ] **Step 6** commit：`feat(gui): 设置视图(表单 + validate + save(SaveTarget)) (Phase3 T5; Spec D §4.3/§5)`
- [ ] **Step 7** 开 PR `desktop-gui` → main(Phase 3),CI 真机绿后 squash 合并(我自管)。

---

## Self-Review(plan vs Spec D §5)

- §5 七视图:仪表盘/备份(Phase 2)+ 复查/初始化/查找/盘列表/设置(本阶段 T4/T3/T2/T1/T5)→ 全覆盖 ✓
- 复查可取消、选盘 → T4 ✓;初始化防呆列表 + force 二次确认 → T3 ✓;查找结果表 → T2 ✓;盘列表 → T1 ✓;设置预览+编辑+validate+save(默认 %APPDATA%)→ T5 ✓
- §3 长任务后台(仅 verify)→ T4 复用 archive 机制 ✓
- 去重:`fmt_gb`/source 文案/进度+日志片段抽到 `views/util.rs`,dashboard/archive 回改引用 ✓
- 测试边界:纯逻辑(格式化/解析/校验/状态映射)单测;渲染靠编译 + 实跑(Spec D §7)✓
- 不改 core(只消费已就绪 API);若发现 API 缺口 → 停下评估,不在 GUI 里塞 core 逻辑。
- 占位符扫描:各 view 给了 UI 状态结构 + API 调用契约 + 测试点(egui 渲染细节按既定 Phase 2 模式落地)。
