# Spec C：Review + 修复 Campaign（直到无 P2 以上）

* 日期：2026-05-29
* 状态：待批准（user-review pending）
* 类型：方法论 / 流程 spec（非功能 spec）
* 关联：[Spec A：Batch 3 严格枚举](./2026-05-28-batch3-strict-enumeration-design.md)、[Spec B：Batch 3.5 压缩包测试](./2026-05-28-batch3.5-archive-integrity-test-design.md)
* 范围声明：本 spec 只定义「如何审、如何修、何时算完」。具体发现项在 campaign 执行期写入独立 ledger（§9），不在本文内。

---

## 1. 目标

对 bftool 做**整体 + 逐模块**审查，迭代修复，直到无 P0/P1/P2（P3 可留）。每个 P2+ 修复配回归测试（TDD 红-绿）。借鉴 conserve / restic / rustic / rsure 的工程教训作为额外审查镜（§7）。桌面版与净新功能**不在**本 campaign。

## 2. 范围

**代码** —— `crates/bftool-core/src/engine/` 下除 `mod.rs` 外的 **11 个模块**：`archive.rs`、`archive_test.rs`、`cruft.rs`、`drive.rs`、`find.rs`、`manifest.rs`、`paths.rs`、`safety.rs`、`status.rs`、`txn.rs`、`verify.rs`；外加 `config.rs`、`reporter.rs`、`lib.rs`；以及 `crates/bftool-cli/`（`main.rs`、`cli.rs`）。

**配置 / 构建** —— `Cargo.toml`（workspace + 各子 crate）、`Cargo.lock`、`bftool.toml.example`、`.github/workflows/build.yml`。理由：CI 打包步骤会把 `bftool.toml.example` 连同 exe 一起发布（build.yml:56），**配置漂移 = 用户拿到的默认配置漂移**，必须审；`Cargo.lock` 的依赖版本与 `cargo audit` 直接相关。

**文档** —— `README.md`、`docs/superpowers/specs/` 下 spec 与代码的一致性。

**4 个维度（全覆盖）** —— ① 正确性 & 错误处理 ② 架构 & 类型设计 ③ 测试覆盖 ④ 文档·CI·spec 一致性。

**spec 的地位** —— Spec A/B 视为「意图真值」：代码与 spec 冲突 = 发现项；但若 spec 本身存在 P2+ 缺陷（逻辑漏洞、自相矛盾），同样记录为发现项（指向修 spec 或修代码，由证据决定）。

**出范围** —— 桌面版（单独 brainstorm → 独立 spec）；净新功能（如 restore 子命令、加密、去重，各开 mini-spec，不混入）；超出「修复所需」的大重写；改动备份盘磁盘格式 / 索引 CSV 列名（向后兼容承诺，见 README「从 PowerShell 旧版迁移」）。

## 3. 严重度分级与停止条件

本 spec 自包含定义（不依赖外部表）：

| 级别 | 含义 | 典型例子 |
|---|---|---|
| **P0 阻断** | 数据丢失 / 误删或损坏源 / 备份盘数据损坏 / 构建无法完成 | 事务顺序错 → 源已移走但索引/数据没落地；`fs::rename` 跨卷失败未回退导致文件丢失 |
| **P1 严重** | 完整性校验被绕过 / 静默失败 / 进程崩溃 / fail-closed 承诺被破坏 | `--unsafe-no-hash` 下漏判未覆盖文件；`unwrap()`/`expect()` 在可触达运行路径 panic；错误被 `.ok()` 吞掉导致「假成功」 |
| **P2 中等** | 边界 case 行为错误 / 错误处理不当 / 缺关键路径测试 / API 设计缺陷 | Windows 长路径（>260）未处理；核心模块（archive/manifest/verify/txn）无测试；`Reporter` 抽象泄漏实现细节 |
| **P3 轻微** | 文案 / 注释 / 风格 / 文档漂移 | README 写 `engine/ui.rs` 实际是 `reporter.rs`；`clippy::pedantic` 风格建议 |

**停止条件** = 连续两轮对抗复审（Phase 4）都**无新增 P0/P1/P2**，且满足下面两个前置。

**前置 1 —— Needs-evidence 清零**：任何一轮对抗复审被计入「无新增」之前，ledger 里**不得有 `Needs-evidence` 状态的 P2+ 候选**。每个都必须先裁决为 `Queued`（补到证据、入修复队列）/ `Deferred-P3`（降级）/ `Rejected`（驳回）/ 转 mini-spec（净新功能），并在 ledger 写明裁决理由（§9 `Resolution rationale`）。否则高风险候选可能一直挂在 `Needs-evidence` 绕过停止条件。

**前置 2 —— 计数重置规则**：只要 Phase 3 又修复了任何 P2+（说明仍有实质问题在流动），「连续无新发现」的轮次计数**清零重算**。

## 4. 角色与 agent 编排（方案 B）

**我（主控）** —— 整体架构审 + 汇总去重分级 + 驱动 TDD 修复 + 终审 + 维护 ledger。

**并行评审 agent**（`Agent` 工具），按维度分派，用**完全限定的 subagent_type**（带 `pr-review-toolkit:` 命名空间）：

| subagent_type | 负责维度 |
|---|---|
| `pr-review-toolkit:code-reviewer` | 正确性、项目规约符合度 |
| `pr-review-toolkit:silent-failure-hunter` | 静默失败、吞错、不当 fallback |
| `pr-review-toolkit:type-design-analyzer` | 类型设计、不变量表达、封装 |
| `pr-review-toolkit:pr-test-analyzer` | 测试覆盖缺口 |
| `pr-review-toolkit:comment-analyzer` | 注释 / 文档与代码漂移 |

**可用性预检与 fallback（Phase 0 执行）** —— 这些是 `pr-review-toolkit` 插件提供的具名 agent。Phase 0 先确认它们在当前环境可派发；**若某环境未装该插件**，fallback 到 `general-purpose`（或 `claude`）通用 agent，并在 prompt 注入等价 persona（要点：该维度审查目标 + 本节输出契约）。即方法论**不硬依赖**插件存在，换环境/换线程仍可复现。

**覆盖矩阵（证明 §2 全范围无遗漏，Finding 2）** —— 维度分派不足以保证每个模块都被审，故附 `文件组 × 负责 agent` 矩阵；每个 agent 必须在输出附 `Reviewed files`（实际读过的文件 + 行范围）。Phase 1 退出门：所有 agent 的 `Reviewed files` 并集**必须覆盖 §2 全部 in-scope 文件**，缺口触发定向补派。

| 文件组 | 主审 | 协审 |
|---|---|---|
| `archive.rs` `manifest.rs` `txn.rs` `verify.rs`（核心数据路径） | code-reviewer · silent-failure-hunter | type-design-analyzer · pr-test-analyzer |
| `cruft.rs` `archive_test.rs` `safety.rs`（枚举/测试器/安全） | code-reviewer · silent-failure-hunter | pr-test-analyzer |
| `drive.rs` `find.rs` `status.rs` `paths.rs`（盘/索引/路径） | code-reviewer | type-design-analyzer · pr-test-analyzer |
| `config.rs` `reporter.rs` `lib.rs`（配置/抽象/门面） | type-design-analyzer | code-reviewer |
| `bftool-cli/`（`main.rs` `cli.rs`） | code-reviewer | silent-failure-hunter |
| `Cargo.toml` `Cargo.lock` `bftool.toml.example` `build.yml`（构建/配置/CI） | 我（主控）+ code-reviewer | — |
| `README.md` + specs↔代码一致性 | comment-analyzer | 我（主控） |

注释/文档漂移（comment-analyzer）与测试缺口（pr-test-analyzer）横跨所有文件组，按上表「协审」列覆盖；架构层（模块边界、core/cli 分层）由我（主控）整体审。

**对抗复审 agent**（Phase 4）—— 独立实例，任务是**尝试推翻**「已修」结论 + 主动找新问题（对齐 ai-regression-testing 原则：别让同一个模型既写修复又给修复打分）。

**Agent 输出契约（强制，保证并行结果可合并）** —— 每条 finding 必须含：

```
Reviewed files（本次实际读过的文件 + 行范围；供覆盖矩阵核对）

每条 finding：
Finding ID | Severity(P0-P3) | Module | Evidence(file:line)
Broken invariant（被破坏的不变量/期望） | Suggested regression test
Confidence(high/med/low) | Duplicate-of（疑似重复的已知 ID，可空）
```

**证据门槛** —— P2+ 若**缺 `file:line` 或可复现证据**，先标 `Needs-evidence`，**不直接进修复队列**；由我补证据确认后才升级入队（或降级 / 驳回）。

## 5. 流程

- **Phase 0 — 绿色基线 + 环境就绪**：
  - **工具就绪**：安装并固定 §6 工具版本（`cargo-audit`、`cargo-llvm-cov`）。「工具未装」是 Phase 0 的**安装任务**，不是 finding、不算红灯 —— 装好后门禁才生效。
  - **agent 预检**：确认 §4 的 `pr-review-toolkit:*` agent 可派发；不可用则登记 fallback 到 `general-purpose` + persona。
  - **绿色基线**：跑通 §6 全部门禁命令，记录当前真实状态（哪些过、哪些红）。没有绿色基线就无法区分「原有问题」和「我引入的问题」。
- **Phase 1 — 并行审**：按 §4 覆盖矩阵分派 agent，收回符合输出契约的 findings。**退出门**：所有 agent 的 `Reviewed files` 并集覆盖 §2 全部 in-scope 文件，缺口定向补派后才进 Phase 2。
- **Phase 2 — 分级 ledger**：**先把已知基线物化进 ledger 文件**（§9，提交入库 → 换线程/换 agent 可复现），再并入 agent findings 去重分级。Seed 来源**必须是 repo 内可引用的 artifact**（Finding 4）：
  - Spec A/B（`docs/superpowers/specs/2026-05-28-*.md`）的已批准不变量与各轮 P 系列修复记录（带 §/行号）
  - 实施计划 `docs/superpowers/plans/2026-05-28-batch3-batch3.5-implementation.md`
  - spec-review / 实施提交区间：`git log` 中自 `15ca218`（计划提交）起的 `docs(spec)` 与 `feat/fix(core)` 提交
  - 当前 CI drift（§6 列出的 5 项已知项）
  - 我首轮整体架构审的发现
  - 理由：不先 seed，campaign 开局会把已知问题当「新发现」反复归类、浪费轮次；用 repo 内 artifact 而非「记忆中的 13 轮」保证可复现。
- **Phase 3 — TDD 修复循环**：每个入队 P2+ → 先写复现失败的测试（红）→ 修（绿）→ 跑 §6 门禁 → 更新 ledger。提交策略见 §8。
- **Phase 4 — 对抗复审**：独立 agent 复审改动 + 全局再扫。有新 P2+ → 回 Phase 3（计数清零）。
- **循环**：Phase 3 ↔ Phase 4，直到 §3 停止条件满足。

## 6. 工具门禁

**当前 CI 现状（build.yml，记为 Phase 0 已知 drift）**：

- `cargo fmt --all -- --check` 是 `continue-on-error: true`（:24）—— 格式仅 advisory，不阻断
- `cargo clippy --workspace --all-targets -- -D warnings` 是 `continue-on-error: true`（:29）—— lint 仅 advisory
- check job 用 `cargo check`（:26），不是 `build --all-targets`
- **无 `cargo test`** —— 测试从不在 CI 跑
- **无 `cargo audit`** —— 依赖漏洞无扫描

**目标硬口径（workspace 级，每轮本地必过 + 补进 CI）**：

```
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo audit
```

**覆盖率（证据，非硬阈值）** —— 报告命令明确产物路径（Finding 2）：

```
cargo llvm-cov --workspace --all-targets --lcov --output-path target/llvm-cov/lcov.info
```

（人工查看可换 `--html`，产物 `target/llvm-cov/html/`）。看趋势：新增 / 修复代码必须被测覆盖，但**不设全局百分比硬门槛**。CI 用 `actions/upload-artifact` 上传 `target/llvm-cov/`。ledger 的 `Verification evidence`（覆盖维度）= 指向报告中**该回归测试覆盖到修复代码行**，而非全局百分比。

**pedantic** —— `clippy::pedantic` 保持 **advisory**，**不**与上面的硬门禁混合（避免风格噪音卡住实质修复）。

**工具安装 / 版本固定 / 产物（Finding 5）** —— `cargo-audit` 与 `cargo-llvm-cov` 非默认自带，必须显式装好再纳入门禁，否则会变成「工具缺失红灯」而非真实发现：

| 工具 | 本地（exact 版本固定，Finding 1） | CI | 版本策略 / 产物 |
|---|---|---|---|
| `cargo-audit` | `cargo install cargo-audit --version =<X.Y.Z> --locked` | `rustsec/audit-check@<tag/sha>`，或 `taiki-e/install-action@<tag/sha>` + `tool: cargo-audit@<X.Y.Z>` | 版本与 action ref 在 Phase 0 冻结；advisory DB 运行时拉取；红灯 = 有已知漏洞 |
| `cargo-llvm-cov` | `rustup component add llvm-tools-preview` + `cargo install cargo-llvm-cov --version =<X.Y.Z> --locked` | `taiki-e/install-action@<tag/sha>` + `tool: cargo-llvm-cov@<X.Y.Z>` | 版本冻结同上；产物见上「覆盖率」命令的 `target/llvm-cov/` |

> `<X.Y.Z>` 与 `<tag/sha>` 是 **Phase 0 冻结的具体值**（取当时 latest stable + action 最新 release），选定后写进 CI YAML 与 ledger 工具版本记录，之后冻结；升级走单独 PR。`--version =X.Y.Z` 是 exact 版本固定，`--locked` 额外锁工具自身依赖。

本地与 CI 用同一组命令口径（§6 顶部），保证「本地过 = CI 过」。工具安装属 Phase 0 设置任务。

## 7. 借鉴审查镜（优质项目教训 → checklist）

- **持久性 / durability** —— 复制、索引 CSV、事务标记落盘后是否 `fsync`（及父目录 fsync）？断电会不会留下「索引说成功、数据其实没刷盘」。（restic / conserve）
- **事务崩溃一致性** —— `txn` 的「写标记 → 移源 → 写索引 → 删标记」，在任意一步崩溃 / Ctrl+C 后能否安全恢复或续传，有无中间态导致重复或丢失。（rustic append-only 思想）
- **校验完整性** —— `verify` 是否覆盖全部损坏类别：缺失 / 多余 / 内容改动 / 枚举错误，与 conserve `validate` / restic `check` 的类别对齐。
- **损坏隔离闭环** —— 异常文件隔离 → 下次自动重传 → 重新校验 + 重测，是否真能打破死循环（不会反复隔离同一文件）。（conserve corruption isolation）
- **可恢复性 / 读回（新增）** —— 成功的定义不止「复制成功 + manifest 一致」，还要验证**备份目标能作为真实恢复源**被完整枚举、逐文件比较、SHA256 校验通过。即使当前没有独立 restore 命令，「能恢复」也必须是隐含 DoD —— 审查时把目标盘当成将来要 restore 的源来质询。（rsure：「未测过的备份不是备份」）
- **Windows 专项（Windows-first，最易踩）** —— >260 长路径（`\\?\` 前缀）、保留名（CON/PRN/NUL/COM1…）、大小写不敏感冲突、junction / symlink 处理、跨卷 `rename` 失败回退、文件被 AV / 其它进程占用锁。

## 8. 修复规约（对齐 CLAUDE.md）

- **TDD** —— 红-绿；每个 P2+ 先有复现失败的测试再修。
- **提交粒度** —— **默认单 finding 单 commit**；若多个 finding 同一根因，允许一个 commit 关闭多个 ID，**但 ledger 必须逐 ID 写清各自的回归测试与验证证据**。
- **commit 规范** —— conventional commits（中文 OK）；结尾带 `Co-Authored-By`；不 `--no-verify`；不 force-push main/master；未明确指示不批量攒 commit。
- **错误处理** —— 不 swallow：不裸 `.ok()` / `let _ =` / `unwrap_or(...)` 吞错；`unwrap()` / `expect()` / `panic!` 只在确有不变量保证处，且注释写明理由。
- **改动节制** —— 只做修复所需的最小改动；顺手发现的超范围问题用 spawn-task / mini-spec 记下，不就地扩张。

## 9. 发现清单（ledger）

字段（含 §4 agent 契约关键信息 + 裁决/验证证据，Finding 6）：

`ID | 模块/文件 | 维度 | 严重度 | 描述 | Broken invariant | 证据(file:line) | Confidence | Duplicate-of | Suggested test | 修复 commit | 回归测试 | Verification evidence | Adversarial round | Resolution rationale | 状态`。

状态取值：`Needs-evidence` / `Queued` / `Fixed` / `Verified` / `Rejected` / `Deferred-P3`。

- `Rejected`（误报 / 不成立）必须填 `Resolution rationale`，**不计入** open P2+；**重复项**用 `状态=Rejected` + `Duplicate-of=<canonical ID>`（`Duplicate-of` 是字段、不是独立状态）。
- `Needs-evidence` 在任何 dry-count 轮次前必须清零（§3 前置 1）。
- `Verified` 必须填 `Verification evidence`（对抗复审确认 + 门禁通过）与 `Adversarial round`（第几轮被确认）。
- 「转 mini-spec」（净新功能、出 campaign 范围）在 ledger 记为 `Rejected` + `Resolution rationale` 指向对应 mini-spec，不计 open P2+。

campaign 执行期实时维护；结束时归档到 `docs/superpowers/2026-05-29-review-fix-campaign-ledger.md`。

## 10. 完成定义（DoD）

1. **确认成立（confirmed-real）的 P0/P1/P2 全部 `Verified`**，且各有对应回归测试 + `Verification evidence`；`Rejected`（含重复项：`状态=Rejected` + `Duplicate-of`）有 `Resolution rationale` 且不计 open P2+。
2. **无 `Needs-evidence` 残留**（每条都已裁决，§3 前置 1）。
3. §6 目标硬口径全绿（本地 + CI 都改成硬门禁）。
4. 连续两轮对抗复审无新 P2+（计数遵守 §3 前置 2 重置规则）。
5. ledger 归档；README / spec 的 drift 已修。
6. P3 可留，但在 ledger 标记 `Deferred-P3` 列出，交用户决定。

## 11. 不做的事

- 桌面版（单独 brainstorm → 独立 spec）。
- 净新功能（restore 子命令、加密、去重等）—— 各开 mini-spec，不在本 campaign。
- 超出修复所需的架构重写。
- 改动备份盘磁盘格式 / 索引 CSV 列名（向后兼容）。
