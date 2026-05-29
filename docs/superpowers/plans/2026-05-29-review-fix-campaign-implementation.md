# Review + 修复 Campaign 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 `superpowers:executing-plans`（inline，主控编排）执行本计划。Phase 0 / 5 是确定性 checkbox 任务；Phase 1–4 是发现驱动循环，per-finding 修复任务在 Phase 2 动态物化进 ledger，循环体形状固定（见 Phase 3 模板）。

**Goal:** 执行 [Spec C](../specs/2026-05-29-review-fix-campaign-design.md) 的 review+修复 campaign，把 bftool 推进到无 P0/P1/P2，每个修复配回归测试。

**Architecture:** 两段式。**Phase 0/5 确定性**（环境、基线、CI 硬化）现在就写死；**Phase 1–4 发现驱动循环**（并行 agent 审 → 分级 ledger → 逐 finding TDD 修 → 对抗复审 → 循环到 dry×2）。ledger 是 Phase 1–4 的动态任务表，循环单元（一个 finding）的 TDD 形状固定。

**Tech Stack:** Rust（cargo workspace）、pr-review-toolkit agents、cargo-audit、cargo-llvm-cov、GitHub Actions。

**隔离策略:** 代码改动在分支 `campaign/review-fix-p2`，`main` 保持干净直到 campaign 完成、可整体复审；**不 push**（等用户要求）。docs（plan/ledger）随 campaign 落同一分支，便于整体复审。

---

## 关键文件结构（campaign 触碰 / 新建）

- 新建 `docs/superpowers/2026-05-29-review-fix-campaign-ledger.md` —— 发现清单（Phase 2 物化，全程维护，§9 字段）
- 修改 `.github/workflows/build.yml` —— CI 硬化（Phase 5，代码绿后再翻硬门禁，避免提交"明知红"的 CI）
- 修改 `crates/bftool-core/src/engine/*.rs`、`config.rs`、`reporter.rs`、`crates/bftool-cli/src/*.rs` —— Phase 3 动态
- 新建 `crates/bftool-core/tests/*.rs`（集成测试）+ 各模块 `#[cfg(test)]`（单测）—— Phase 3 动态，按 Spec B §6 + 发现

---

## Phase 0 — 绿色基线 + 环境就绪（确定性）

### Task 0.1: 建 campaign 分支

**Files:** 无（git）

- [ ] **Step 1: 从 main 建分支**

```bash
git switch -c campaign/review-fix-p2
```
Expected: `Switched to a new branch 'campaign/review-fix-p2'`

- [ ] **Step 2: 确认干净**

```bash
git status --short
```
Expected: 空输出

### Task 0.2: 工具就绪（exact 版本固定）

**Files:** 无（工具安装）

- [ ] **Step 1: 记录 toolchain**

```bash
cargo --version && rustc --version && rustup component add llvm-tools-preview
```

- [ ] **Step 2: 查 crates.io 当前 latest stable，冻结版本号，exact 安装**

```bash
cargo install cargo-audit --version =<冻结版> --locked
cargo install cargo-llvm-cov --version =<冻结版> --locked
```
`<冻结版>` = 执行时查到的当前 latest stable（如 `cargo install --list` / crates.io）。把两个具体版本号写进 ledger「工具版本」表（Spec C Finding 1 的冻结点落地）。

### Task 0.3: 绿色基线（记录现状，不修复）

**Files:** 无（只跑命令记录）

- [ ] **Step 1: 逐条跑 §6 门禁，记录 pass/fail（红的即"已知 drift"，不算 campaign 引入）**

```bash
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo audit
cargo llvm-cov --workspace --all-targets --lcov --output-path target/llvm-cov/lcov.info
```

- [ ] **Step 2: 把每条结果写进 ledger「Phase 0 基线」段**（命令 → pass/fail → 摘要；覆盖率记基线数字）。这是后续区分"原有问题 vs 我引入"的锚点。

### Task 0.4: agent 预检 + 创建 ledger 文件

**Files:** Create `docs/superpowers/2026-05-29-review-fix-campaign-ledger.md`

- [ ] **Step 1: 试派一个 pr-review-toolkit agent 确认可派发**（轻量任务，如让 `pr-review-toolkit:code-reviewer` 审 `paths.rs` 这种最小模块）。可用 → 记 OK；不可用 → ledger 记 `fallback=general-purpose+persona`。

- [ ] **Step 2: 建 ledger 文件**，含表头（§9 字段）+「工具版本」段（Task 0.2）+「Phase 0 基线」段（Task 0.3）+「环境/agent」段（本 task）。

- [ ] **Step 3: commit Phase 0**

```bash
git add docs/superpowers/2026-05-29-review-fix-campaign-ledger.md
git commit -m "chore(campaign): Phase 0 基线 + 工具固定 + ledger 初始化" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Phase 1 — 并行审（发现驱动，按 §4 覆盖矩阵）

过程（非固定 task；产出是 findings，写进 ledger）：

- [ ] **Step 1: seed 已知基线进 ledger**（Spec C §5 Phase 2 + Finding 4）。读 repo 内 artifact 并各记一条 seed：
  - Spec A/B 已批准不变量 + 各轮 P 系列修复（`docs/superpowers/specs/2026-05-28-*.md`，带 §/行号）
  - 实施计划 `docs/superpowers/plans/2026-05-28-batch3-batch3.5-implementation.md`
  - `git log 15ca218..HEAD` 的 spec/feat/fix 提交
  - Phase 0 基线里红的项（CI drift 5 项 + 任何红门禁）
- [ ] **Step 2: 按覆盖矩阵并行派 agent**（一条消息多个 `Agent` 调用），每个 agent 喂：目标文件组 + Spec C §4 输出契约（强制含 `Reviewed files` + 每条 finding 的 `Finding ID/Severity/Module/Evidence(file:line)/Broken invariant/Suggested test/Confidence/Duplicate-of`）+ Spec A/B/C 作为不变量参照 + §7 审查镜 checklist。
- [ ] **Step 3: Phase 1 退出门**——汇总所有 agent 的 `Reviewed files`，并集必须覆盖 §2 全部 in-scope 文件；缺口定向补派后才进 Phase 2。

**agent 分派 prompt 要点（每个 agent 通用骨架）：** "你在审 bftool（Rust 备份工具，Windows-first）。只读你被分配的文件组：<files>。按维度 <dimension> 找问题。每条 finding 必须给 `file:line` 证据 + 被破坏的不变量 + 建议的回归测试 + confidence。无 file:line 的标 low confidence。额外用这些镜子查：<§7 相关镜>。最后列 `Reviewed files`（文件:行范围）。参照不变量见 Spec A/B/C（已读）。输出结构化，不要改任何代码。"

---

## Phase 2 — 分级 ledger（发现驱动）

- [ ] **Step 1: 去重**——按 `(file, 行附近, 不变量)` 合并 agent findings + seed；重复项用 `状态=Rejected` + `Duplicate-of=<canonical ID>`。
- [ ] **Step 2: 分级**——每条定 P0/P1/P2/P3（§3 表）。
- [ ] **Step 3: 证据门槛**——P2+ 缺 `file:line`/不可复现 → `Needs-evidence`；我补证据后裁决为 `Queued`/`Deferred-P3`/`Rejected`/转 mini-spec（净新功能用 `mcp__ccd_session__spawn_task` 或新 mini-spec 记下，记 `Rejected`+rationale）。
- [ ] **Step 4: 入队**——所有 `Queued` 的 P2+ 按 P0>P1>P2 排序，进 Phase 3。

---

## Phase 3 — TDD 修复循环（每个 Queued finding 走同一模板）

**每个 finding 的固定 5 步（这是模板，`<...>` 按具体 finding 实例化）：**

- [ ] **Step 1: 写复现失败的测试**（红）

```rust
// 在 <目标模块> 的 #[cfg(test)] 或 crates/bftool-core/tests/<name>.rs
#[test]
fn <finding_id>_<被破坏不变量的简述>() {
    // Arrange: 构造触发该 finding 的输入/环境（用 tempfile 等）
    // Act: 调用 <被测函数>
    // Assert: 断言"正确不变量"应成立 —— 当前实现会让它 FAIL
}
```

- [ ] **Step 2: 跑测试确认 FAIL**

```bash
cargo test --workspace <finding_id> -- --nocapture
```
Expected: FAIL（证明确实复现了 finding；若 PASS 说明 finding 不成立 → ledger 记 `Rejected`+rationale）

- [ ] **Step 3: 最小修复**——改 `<file:line>`，只动修复所需。遵守 §8：不 swallow 错误、panic 路径写理由、最小改动。

- [ ] **Step 4: 跑门禁确认 GREEN**

```bash
cargo test --workspace <finding_id>     # 新测试过
cargo test --workspace --all-targets    # 无回归
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: 全 PASS

- [ ] **Step 5: commit + 更新 ledger**

```bash
git add <改动文件> <测试文件>
git commit -m "fix(<scope>): <finding 简述> (ledger <finding_id>)" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
ledger 该行填 `修复 commit`、`回归测试`、状态 → `Fixed`。同根因可一个 commit 关多个 ID，但逐 ID 写测试/证据（§8）。

> 循环：队列里每个 P0/P1/P2 都走完上面 5 步 → 进 Phase 4。

---

## Phase 4 — 对抗复审（发现驱动）

- [ ] **Step 1: 派独立 agent 对抗复审**——对每个本轮 `Fixed` 的 finding，派一个**新**的独立 agent，任务是**尝试推翻**"已修"（找反例、漏掉的边界、修复引入的新问题）。agent 不知道是谁修的（对齐 ai-regression-testing）。
- [ ] **Step 2: 全局再扫**——同时派覆盖矩阵 agent 再扫改动涉及的文件组 + 一个 completeness-critic agent 问"还缺什么维度/未验证的声明"。
- [ ] **Step 3: 裁决**——
  - 推翻成立 → finding 退回 `Queued`，回 Phase 3。
  - 推翻不成立 + 门禁绿 → finding `Verified`，填 `Verification evidence` + `Adversarial round`。
  - 新发现 → 进 Phase 2 分级。
- [ ] **Step 4: 停止判定**——本轮无新 P0/P1/P2 **且** ledger 无 `Needs-evidence` 残留 → dry 计数 +1；否则清零（§3 前置）。`dry==2` → 进 Phase 5；否则回 Phase 1/3。

---

## Phase 5 — 收尾（确定性，代码绿后执行）

### Task 5.1: CI 硬化（build.yml）

**Files:** Modify `.github/workflows/build.yml`

- [ ] **Step 1: 改 check job** —— 去掉 `cargo fmt`（:24）和 `cargo clippy`（:29）的 `continue-on-error`；check job 的 `cargo check`（:26）升级为 `cargo build --workspace --all-targets`；新增 `cargo test --workspace --all-targets` step。
- [ ] **Step 2: 新增 audit + coverage step** —— `rustsec/audit-check@<pinned-sha>`；`taiki-e/install-action@<pinned-sha>` 装 `cargo-llvm-cov@<冻结版>` 后跑 llvm-cov + `actions/upload-artifact` 上传 `target/llvm-cov/`。版本号取 Task 0.2 冻结值。
- [ ] **Step 3: 验证** —— 本地 yaml 结构核对 + 命令与 Task 0.3 逐字一致（CI 无法纯本地跑绿，真实验证在用户 push 后；本计划不 push）。ledger 记"CI 硬化已提交，待 push 验证"。
- [ ] **Step 4: commit**

```bash
git add .github/workflows/build.yml
git commit -m "ci: fmt/clippy 转硬门禁 + 加 test/audit/coverage" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 5.2: 文档 drift 修正

**Files:** Modify `README.md`（+ 任何 spec↔代码 drift）

- [ ] **Step 1:** 按 Phase 1 comment-analyzer 的 drift findings 修 README（已知：项目结构图 `engine/ui.rs` → 实际 `reporter.rs`）+ 其它确认的 drift。
- [ ] **Step 2: commit**

```bash
git add README.md
git commit -m "docs: 修正 README 与代码漂移（reporter 等）" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 5.3: DoD 终检 + ledger 归档

- [ ] **Step 1: 跑全套门禁，全绿**（§6 五条 + 覆盖率）。
- [ ] **Step 2: 核 DoD（§10）**——所有确认 P0/P1/P2 = `Verified`；无 `Needs-evidence`；dry×2 达成；Rejected/Duplicate 有 rationale。
- [ ] **Step 3: ledger 终态归档**——`Deferred-P3` 列表交用户;在 ledger 写 campaign 总结（修了几个、各级数量、CI 待 push）。
- [ ] **Step 4: commit ledger 终态**，然后向用户汇报：分支 `campaign/review-fix-p2` 待复审/合并/push。

---

## Self-Review（计划 vs Spec C）

- §2 范围 → Phase 1 覆盖矩阵 + 退出门覆盖全部 in-scope 文件 ✓
- §3 严重度/停止条件/重置 → Phase 2 分级 + Phase 4 Step 4 停止判定 ✓
- §4 agent 编排/契约/证据门槛 → Phase 1 Step 2 + Phase 2 Step 3 ✓
- §5 五阶段 → Phase 0–4 一一对应（CI 移到 Phase 5 避免提交明知红的 CI）✓
- §6 工具门禁 → Phase 0.2/0.3（本地）+ Phase 5.1（CI）✓
- §7 审查镜 → Phase 1 agent prompt 注入 ✓
- §8 修复规约 → Phase 3 模板（TDD/最小改动/commit 规范）✓
- §9 ledger → Phase 0.4 建文件 + 全程维护 ✓
- §10 DoD → Phase 5.3 ✓
- §11 不做 → Phase 2 Step 3 净新功能转 mini-spec ✓
