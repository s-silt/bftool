# 桌面版 Phase 1：core API 塑形 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: `superpowers:executing-plans`(inline)或 `subagent-driven-development`。Steps 用 `- [ ]`。每个 task 独立 TDD(红-绿)+ 单 commit;每步后跑 §门禁。**cargo 用 PowerShell 跑**(git-bash 会误抓 coreutils link)。

**Goal:** 把 [Spec D](../specs/2026-05-29-desktop-gui-design.md) §4 的 core API 改动落地——结构化查询、计划/执行分离、类型化盘、配置读写、last_verify、取消钩子——让 GUI 能"读结构 + 丢意图给后台线程",**且 CLI 行为不变、79 测试不回归**。本阶段**不写 GUI**。

**Architecture:** 全在 `bftool-core` 内加结构化 API + 类型;CLI(`bftool-cli`)改为消费这些结构(渲染层保留)。每项配回归测试(复用 `temp_world` 集成脚手架 + 已有单测基建)。

**Tech Stack:** Rust workspace、anyhow、serde、csv、chrono、tempfile(dev)。

**门禁(每步)**:`cargo test --workspace --all-targets` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check`,全绿。

**隔离**:基于 main(campaign 已合并)开分支 `desktop-gui`(已存在);Phase 1 提交其上,完成开 PR → main(我自管 git)。

---

## 文件结构(Phase 1 触碰)

- `crates/bftool-core/src/engine/drive.rs` — `BackupDrive`/`WritableDrive`/`DriveError`、`try_into_writable`、`init_candidates`、`InitCandidate`
- `crates/bftool-core/src/config.rs` — `LoadedConfig`/`ConfigSource`/`SaveTarget`/`save`
- `crates/bftool-core/src/engine/verify.rs` — `VerifyReport.issues/extras`、`VerifyIssue`/`ExtraFile`、`VerifyOutcome`、取消钩子
- `crates/bftool-core/src/engine/archive.rs` — `ArchivePlan`/`PlanItem`/`PlanAction`、`plan()`/`run_plan()`、取消钩子;`run = plan()+run_plan()`
- `crates/bftool-core/src/engine/status.rs` — `status::gather -> StatusReport`(+ `LastVerify`)
- `crates/bftool-core/src/engine/find.rs` — `find::search -> Vec<FindMatch>`
- `crates/bftool-core/src/engine/verify_state.rs`(新建) — `复查记录.csv` 读写(`record_verify`/`read_last_verify`)
- `crates/bftool-core/src/engine/paths.rs` — `system_verify_log` 路径常量
- `crates/bftool-cli/src/cli.rs` + `terminal_reporter.rs` — 消费新结构,渲染层保留

**任务顺序**(按依赖):T1 类型化盘 → T2 init_candidates → T3 LoadedConfig → T4 取消钩子 → T5 verify 明细+取消 → T6 last_verify → T7 archive plan/run_plan → T8 status/find 结构化 → T9 CLI 适配 + 集成。

---

### Task 1：类型化盘 `BackupDrive` / `WritableDrive`(Spec §4.2,落地 L-021)

**Files:** Modify `crates/bftool-core/src/engine/drive.rs`

设计:`DriveInfo` 重命名/保留为 `BackupDrive`(通用,带 `sealed`);新增轻量包装 `WritableDrive(BackupDrive)`(不变量:非封盘且容量达标)。`scan_mounted`/`info_by_letter` 仍返回 `BackupDrive`。

- [ ] **Step 1（红）** 在 drive.rs `#[cfg(test)]` 加：
```rust
#[test]
fn try_into_writable_rejects_sealed_and_small() {
    let sealed = di_full("E", 500, true);   // helper:构造 BackupDrive
    assert!(sealed.try_into_writable(200).is_err());
    let small = di_full("F", 8, false);
    assert!(small.try_into_writable(200).is_err());
    let ok = di_full("E", 500, false);
    assert!(ok.try_into_writable(200).is_ok());
    assert_eq!(ok.try_into_writable(200).unwrap().as_ref().letter, "E");
}
```
- [ ] **Step 2** 跑 → FAIL(类型/方法不存在)。
- [ ] **Step 3（绿）** 实现:`pub type BackupDrive = DriveInfo;`(最小侵入,先别名;或直接把 `DriveInfo` 改名为 `BackupDrive` 并加 `pub use` 别名保 CLI 兼容)。加：
```rust
#[derive(Debug, Clone)]
pub struct WritableDrive(BackupDrive);
#[derive(Debug)]
pub enum DriveError { Sealed, TooSmall { total_gb: u64, min_gb: u64 } }
impl std::fmt::Display for DriveError { /* 文案带"怎么修" */ }
impl BackupDrive {
    pub fn try_into_writable(self, min_drive_gb: u64) -> Result<WritableDrive, DriveError> {
        if self.sealed { return Err(DriveError::Sealed); }
        let min = min_drive_gb.saturating_mul(1<<30);
        if self.total_bytes < min { return Err(DriveError::TooSmall{ total_gb: self.total_bytes>>30, min_gb: min_drive_gb }); }
        Ok(WritableDrive(self))
    }
}
impl WritableDrive { pub fn as_ref(&self) -> &BackupDrive { &self.0 } pub fn into_inner(self)->BackupDrive{self.0} }
```
- [ ] **Step 4** 跑 test + clippy + fmt → 绿。
- [ ] **Step 5** commit：`refactor(core): 类型化盘 BackupDrive + WritableDrive::try_into_writable (Spec D §4.2 / L-021)`

> 注:archive 写路径改吃 `WritableDrive` 放到 T7(避免一次改太多)。本 task 先把类型+方法+测试立住,CLI/archive 暂用 `BackupDrive`(别名=DriveInfo,零破坏)。

---

### Task 2：`drive::init_candidates`(Spec §4.2,Finding #4)

**Files:** Modify `crates/bftool-core/src/engine/drive.rs`

把 `init` 内联的防呆判定(系统盘/资料库盘/非空盘)抽成纯/半纯查询。

- [ ] **Step 1（红）** 加结构 + 测试(用 tempdir 造假盘根 + cfg):
```rust
pub struct InitCandidate { pub letter: String, pub total_gb: u64, pub is_system: bool, pub is_library: bool, pub non_empty: bool, pub can_init: bool, pub block_reason: Option<String> }
#[test]
fn init_candidate_blocks_library_drive() { /* cfg.ready_root 在 X: → 该盘 can_init=false, block_reason 含"资料库" */ }
```
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** 抽出判定:复用现有 `system_drive_letter()`/`qualifier_letter()`/非空检查逻辑(从 `init` 提取成 `fn classify_for_init(letter,total,root,cfg)->InitCandidate`);`init_candidates(cfg)` 枚举挂载盘(sysinfo)逐个分类。`init()` 改为调用同一 `classify_for_init` 做防呆(去重),`--force` 仍跳过。
- [ ] **Step 4** test + clippy + fmt 绿。
- [ ] **Step 5** commit：`feat(core): drive::init_candidates 结构化初始化防呆 (Spec D §4.2)`

---

### Task 3：`LoadedConfig` / `ConfigSource` / `save`(Spec §4.3)

**Files:** Modify `crates/bftool-core/src/config.rs`;Modify `crates/bftool-cli/src/cli.rs`(取 `.config`)

- [ ] **Step 1（红）** 加类型 + 测试:
```rust
pub enum ConfigSource { Explicit(PathBuf), Candidate(PathBuf), Default }
pub struct LoadedConfig { pub config: Config, pub source: ConfigSource }
pub enum SaveTarget { CurrentSource, AppData, CurrentDir, Custom(PathBuf) }
#[test]
fn save_to_appdata_roundtrips() { /* tempdir 当 APPDATA;save(AppData) 后 from_path 读回三根目录一致 */ }
#[test]
fn load_default_when_no_file() { /* 候选都不存在 → source==Default */ }
```
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** `Config::load_with_source(explicit) -> Result<LoadedConfig>`(现 `load` 逻辑 + 记 source);`Config::load` 保留为 `load_with_source(..).map(|l| l.config)`(CLI 零破坏)。`LoadedConfig::save(target)`:解析目标路径(AppData=`%APPDATA%\bftool\config.toml`,CurrentDir=`cwd\bftool.toml` 绝对化,CurrentSource=原 source 路径,Default source 拒绝 CurrentSource),`validate()` 后 `toml::to_string_pretty` 写盘(用 `durable::write_synced` 落盘)。
- [ ] **Step 4** test + clippy + fmt 绿。
- [ ] **Step 5** commit：`feat(core): LoadedConfig + save(SaveTarget),默认 %APPDATA% (Spec D §4.3)`

---

### Task 4：取消钩子(Spec §4.5)

**Files:** Modify `archive.rs`、`verify.rs`(签名加 `cancel: &AtomicBool`);CLI 传常量

- [ ] **Step 1（红）** 加测试:verify 在 cancel 预置为 true 时立即返回 `Cancelled`(verify_tree 顶层检查)。
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** `verify_tree`/`run` 接 `cancel: &AtomicBool`,在 manifest/项目/文件循环处 `if cancel.load(Relaxed) { return Cancelled }`。CLI 传 `&AtomicBool::new(false)`。引入 `VerifyOutcome::Cancelled`(见 T5)。archive 的 cancel 在 T7 接(项目边界)。
- [ ] **Step 4** 绿。
- [ ] **Step 5** commit：`feat(core): verify 取消钩子(cancel: &AtomicBool)(Spec D §4.5)`

---

### Task 5：`VerifyReport` 明细 + 项目上下文(Spec §4.1,Finding #2/#3)

**Files:** Modify `crates/bftool-core/src/engine/verify.rs`

- [ ] **Step 1（红）** 扩结构 + 测试(已有 verify_tree 测试基建):
```rust
pub enum VerifyIssueKind { Missing, SizeMismatch, Corrupt, Unverifiable, EnumError }
pub struct VerifyIssue { pub project: String, pub rel: String, pub kind: VerifyIssueKind }
pub struct ExtraFile { pub project: String, pub rel: String }
// VerifyReport 加 issues: Vec<VerifyIssue>, extras: Vec<ExtraFile>
#[test]
fn verify_tree_issue_carries_project_and_kind() {
    // 两个项目各有同名 rel "a.txt",一个损坏 → issue.project 能区分;kind==Corrupt
}
```
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** verify_tree 每处 `report.bad += 1` 旁 push `VerifyIssue{project: project_name.clone(), rel, kind}`;extra 处 push `ExtraFile`。`checked/bad/extra` 计数保留(= 各 vec 派生或并存)。
- [ ] **Step 4** 绿。
- [ ] **Step 5** commit：`feat(core): VerifyReport 结构化明细 issues/extras(带项目上下文)(Spec D §4.1)`

---

### Task 6：`last_verify` 持久化到 system_root(Spec §4.4)

**Files:** Create `crates/bftool-core/src/engine/verify_state.rs`;Modify `paths.rs`、`verify.rs`、`mod.rs`

- [ ] **Step 1（红）** verify_state 测试:
```rust
// record_verify(system_root, drive_id, VerifyOutcome::IssuesFound{bad:2}) → read_last_verify 读回 status/when 一致;缺文件→None
```
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** `VerifyOutcome = Clean | IssuesFound{bad} | ExtraOnly{extra} | Cancelled`;`LastVerify{drive_id, when, status}`。`record_verify(system_root, drive_id, outcome)` 追加/更新 `备份系统\复查记录.csv`(drive_id 去重保最新);`read_last_verify(system_root, drive_id)`。verify::run **跑完一轮**(非取消/非出错)调 `record_verify`(Cancelled 不记)。
- [ ] **Step 4** 绿。
- [ ] **Step 5** commit：`feat(core): 复查结果记 system_root\复查记录.csv,verify 仍对盘只读 (Spec D §4.4)`

---

### Task 7：`archive::plan` / `run_plan`(Spec §4.1,Finding #1/#2 + 冻结决策 + 重验)

**Files:** Modify `crates/bftool-core/src/engine/archive.rs`(最大 task;把 `run` 的 per-project 决策拆成 plan + execute)

- [ ] **Step 1（红）** `temp_world` 集成测试:
```rust
// plan() 对 1 个正常项目 + 1 个空源 → items[0].action==Archive{dest_name}, items[1].action==Skip(reason 含"无可备份")
// run_plan(plan) 执行 → 正常项目归档+移源,空源不动;dest_name == plan 里冻结的名
// run_plan 重验:把 drive 改成 sealed 后再 run_plan(旧 plan) → 该项 Skip("计划已过期")不写盘
```
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** 重构:抽 `decide(cfg,&drive,proj,opts)->PlanAction`(现 handle_one 前半:稳定性/0文件/容量/重名/dry 判定);`plan(cfg,opts)` = 选盘(`pick_active`→`try_into_writable`)+ 对每项 `decide` → `ArchivePlan`。`run_plan(cfg, plan, cancel, reporter)`:逐 item,**项目边界检查 cancel**;**重验安全前置**(盘在/未封盘/容量够/源稳定/同名不冲突——任一变 → Skip "计划已过期");通过则执行现有提交流程(manifest/复制/diff/源复核/txn);累计 `ArchiveSummary{handled,failed,cancelled,sealed_stopped}`。`run = { let p=plan()?; run_plan(p) }`。`PlanAction`/`ArchivePlan`/`PlanItem` 如 spec §4.1。
- [ ] **Step 4** 绿(happy-path/空源/corrupt-catalog 等既有集成测试须仍过)。
- [ ] **Step 5** commit：`refactor(core): archive 拆 plan()+run_plan(),冻意图重验安全前置 (Spec D §4.1)`

---

### Task 8：`status::gather` / `find::search` 结构化(Spec §4.1,L-031)

**Files:** Modify `status.rs`、`find.rs`

- [ ] **Step 1（红）** `find::search(cfg,kw)->Result<Vec<FindMatch>>` 测试(temp 全局索引 csv → 命中返回结构);`status::gather(cfg)->Result<StatusReport>` 测试(StatusReport 字段含 pending_count/current_drive/last_verify/drives)。
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** 把 find::run 的查询逻辑抽成 `search` 返回 `Vec<FindMatch{folder,drive_id,in_drive_path}>`(run 改为 search + 渲染);status 同理 `gather`(读盘 scan + pending 数 + `read_last_verify`)。
- [ ] **Step 4** 绿。
- [ ] **Step 5** commit：`feat(core): status::gather / find::search 结构化查询 (Spec D §4.1 / L-031)`

---

### Task 9：CLI 适配 + 整体门禁

**Files:** Modify `crates/bftool-cli/src/cli.rs`、`terminal_reporter.rs`

- [ ] **Step 1** CLI 各子命令改为消费新结构(`find` 渲染 `Vec<FindMatch>`、status 渲染 `StatusReport`、archive dry-run 渲染 `ArchivePlan`、config-show 渲染 `LoadedConfig`、verify 渲染 `VerifyReport.issues`),**输出文案尽量不变**;传 cancel 常量。
- [ ] **Step 2** `cargo test --workspace --all-targets` + clippy + fmt 全绿;**手动跑 `cargo run -p bftool-cli -- ` 各子命令冒烟**(无盘环境下看文案不崩)。
- [ ] **Step 3** commit：`refactor(cli): 消费 core 结构化 API,渲染层保留,行为不变`
- [ ] **Step 4** 开 PR `desktop-gui` → main(Phase 1),CI 实跑;绿后合并(我自管)。

---

## Self-Review(plan vs Spec D §4)

- §4.1 结构化查询 → T8(status/find)+ T5(verify 明细)✓;archive::plan/run_plan → T7 ✓;VerifyReport 明细 → T5 ✓
- §4.2 类型化盘 → T1;init_candidates → T2 ✓
- §4.3 LoadedConfig/save → T3 ✓
- §4.4 last_verify(system_root,只读盘)→ T6 ✓
- §4.5 取消钩子 → T4(verify)+ T7(archive 项目边界)✓
- CLI 渲染层保留、行为不变 → T9 ✓
- **不在 Phase 1**:GUI(eframe/视图/GuiReporter)→ Phase 2/3 独立 plan。
- 占位符扫描:无 TBD;代码块给了签名 + 测试断言(实现是对现有代码的抽取/扩展,执行时读当前文件)。
- 类型一致性:`BackupDrive`/`WritableDrive`/`VerifyOutcome`/`LastVerify`/`ArchivePlan`/`PlanAction` 跨 task 命名一致。
