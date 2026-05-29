# Review + 修复 Campaign — 发现清单 (Ledger)

* 关联 spec：[Spec C 方法论](specs/2026-05-29-review-fix-campaign-design.md)、实施计划：[plans/2026-05-29](plans/2026-05-29-review-fix-campaign-implementation.md)
* 分支：`campaign/review-fix-p2`
* 状态：Phase 3（TDD 修复）进行中。MSVC 已就位,门禁 fmt/build/test/clippy 全绿;cargo-llvm-cov 已装,cargo-audit 待装(留 Phase 5)。
* 字段：`ID | 模块 | 维度 | 严重度 | 描述 | Broken invariant | 证据 | 来源 | 状态`

---

## 进度(实时,Phase 3)

测试数:0 → **79 passed**。每条修复后门禁(fmt/build/test/clippy)全绿;CI 已硬化(`0792d66`)。

> **⚠ 重大发现 L-044(P1,集成测试新抓到)**:`manifest::build` 与 `verify_tree` 对 base 做 `canonicalize()`(Windows 加 `\\?\` 前缀),而 `cruft::walk` 的 `WalkDir` 产出路径不带前缀 → `strip_prefix` 失配 → rel 退化成**绝对路径** → **diff/verify 在 Windows 上必然失败,核心归档功能根本跑不通**。因 CI 不跑 `cargo test`(CI-4)+ 核心零集成测试,一直未暴露。已修(base 改用传入 root)+ handle_one happy-path 集成测试锁定。commit `0582389`。

**已修复(Fixed,待 Phase 4 对抗复审升 Verified):**

| ID | 严重 | commit | 回归测试 |
|---|---|---|---|
| L-001 | P0 | `810e38e` | durable::write_synced ×2 |
| L-004 | P1 | `6473cab` | source_changed no_hash ×3 |
| L-002 | P2 | `cf0acf2` | diff 对称 ×3 |
| L-006 | P1 | `76c0448` | usable_drives ×4 |
| L-007 | P1 | `b37e2fe` | verify_tree 损坏检测 ×5 |
| L-013 | P2 | `a9de19a` | verify size=0 ×2 |
| L-008 | P2 | `5662795` | verify_disabled 真值表 ×1 |
| L-023 | P2 | `800c36e` | drive_letter_of/qualifier 字符安全 ×2 |
| L-026 | P2 | `800c36e` | check_paths/is_inside ×6 |
| L-003 | P2 | `672d890` | handle_one 空源(脚手架 temp_world)×1 |
| L-017 | P2 | `e694c69` | dup 读失败保守 ×1 |
| L-018 | P2 | `a0d2b90` | note_manual 非致命 ×1 |
| L-024 | P2 | `31d7a4b` | Diff.ok 派生(重构) |
| L-025 | P2 | `20af56a` | Config::validate ×3 |
| L-009 | P2 | `14a78b8` | classify_exit strict ×1 |
| L-005 | P2 | `0582389` | folder_stats fail-closed + happy-path ×1 |
| **L-044** | **P1** | `0582389` | **canonicalize rel 失配(核心校验必败)** + happy-path 锁定 |
| L-014 | P2 | `d66c9b1` | copy_folder 原子复制 + cruft .part ×2 |
| L-022 | P2 | `728dc76` | PendingTxn TOML 往返,删 grab_field ×2 |
| L-027 | P2 | `4691c7d` | next_number/parse_drive_number ×2 |
| L-020 | P2 | `2eb7005` | VerifyStatus enum ×1 |
| L-010 | P2 | `e3b9a86` | 跨卷 fail-closed + safety 文案 ×1 |
| L-032–040 | P3 | `ee5869e` | 文档漂移批量(README/toml/mod/cli/reporter/manifest) |

**严重度修正(主控复核;拟交 Phase 4 确认):**
- L-003 / L-005 / L-008 / L-009:P1 → **P2**。复核后均非"当前可触发的数据丢失/校验绕过",而是 fail-closed 缺口/维护性风险(L-003 空源假成功但不删数据;L-005 跨卷失败可经 check_pending_txn 恢复、不丢数据;L-008 core+cli 双 guard 当前都在、仅未来漂移风险;L-009 仅 no_hash 且 exit code=1 罕见)。仍会修。
- L-002:P1 → **P2**。清单一致 + rel 唯一时缺文件必被 count/extra 捕获;真正静默漏检需 cruft 不对称 → 属脆弱性 + reason 误导。已修。

**新增发现:**
- L-043 → **已由 L-044 修复**(同根因 canonicalize/strip_prefix;verify_tree 与 manifest::build 的 base 都已改)。
- L-044(P1)→ 见上「重大发现」,已修 `0582389`。

**剩余 P2/P3(已评估,见下「取舍」):** L-021、L-011、L-012(P2);L-016/L-031/L-039/L-041/L-042/L-028/029/030(P3)。

## Phase 4 对抗复审结果(2 个独立 agent)

code-reviewer + silent-failure-hunter 各自独立尝试推翻这批修复 → **绝大多数被逐条背书 sound**。发现的真残留已修(commit `3f71c7f`):
- F-01/F-6(P2):verify 清单行缺 Size 且缺 Hash → 只查存在性的 fail-open,已改 fail-closed(+测试)。
- F-4(P2,修订 L-017):本盘索引损坏时原"假设重名+改名续写"会每轮全量重写填盘 → 已改 fail-closed Skipped(+集成测试)。
- F-3(P2):孤儿 `.bftool-part` 不清理 → 已加清理。
- F-5(P3):build doc 跟进 L-019。
- **F-2(P1 质疑,接受为已知残留):L-001 的 fsync 是文件级真改进,但未做父目录 fsync,非 NTFS 介质(exFAT/FAT32)断电仍可能丢目录项。README 已强制备份盘用 NTFS(元数据日志兜底);非 NTFS 目录项 durability 记为残留(真闭合需 FILE_FLAG_BACKUP_SEMANTICS 目录 fsync = FFI,deferred)。L-001 如实表述为「文件级 fsync + 提交顺序 + NTFS 假设」,而非「P0 完全闭合」。**
- F-1(P1 活锁)→ 主控复核**驳回**:copy_folder 会重传缺失文件、无法复制的会 bail 报错,不会静默活锁。
- F-04 / F-02(P3)→ 评估为非 fail-open(经 run catch 优雅降级 / 均 Skipped 非假成功),接受不改。

## 取舍(剩余 P2/P3 处置,交用户拍板)

- **L-021 / L-011 / L-012(P2)**:L-021 是最大的类型重构(`DriveInfo`→`WritableDrive`/`SealedDrive` 拆分,贯穿 drive/archive/verify),价值是"塑造 GUI 直接调用的 core API"——建议**与桌面版设计一并做**(届时才知 GUI 需要什么 API 形状),而非现在盲改。L-011/L-012 是事务恢复的微秒级崩溃窗口边角,数据已被 txn 标记机制守住(不丢、提示人工),价值低改动深,建议记 backlog。
- P3 多为文档/小测,可随手清或留。

## 下一步:Phase 5(CI 已硬化 `0792d66`,待 push 实跑)→ 桌面版设计

---

## 工具版本（Phase 0 冻结点）

| 工具 | 版本 | 状态 |
|---|---|---|
| rustc / cargo | stable-aarch64-pc-windows-msvc | 已装 |
| cargo-audit | 待定 | **待 MSVC**（需编译） |
| cargo-llvm-cov | 待定 | **待 MSVC**（需编译） |

## Phase 0 基线（环境就绪状态）

| 门禁 | 结果 | 备注 |
|---|---|---|
| `cargo fmt --all -- --check` | **PASS** | 格式干净 |
| `cargo build --workspace --all-targets` | **BLOCKED** | 本机初始无 MSVC 链接器 + 无 Windows SDK；用户正在装 MSVC BuildTools(ARM64 VC + Win11 SDK)。SDK 已就位,VC 工具链装中 |
| `cargo test` / `clippy` / `audit` / `llvm-cov` | **BLOCKED** | 同上,待 MSVC |

> 环境发现(见 L-ENV)：本机 ARM64 Windows,首次发现项目从未本地链接过(`target/` 仅有不需链接器的 `.rlib`)。campaign 的 TDD 修复循环依赖可编译,故 Phase 3 起需 MSVC。

## Seed（已知基线，Phase 2 起点）

- **Spec A/B 意图**：严格枚举(cruft 感知 walker)、压缩包完整性测试(7-Zip/WinRAR `t`)、fail-closed(no_hash 必配 archive test)—— 作为"代码应满足"的对照。
- **前序提交**：`git log` 自 `15ca218`(计划)起的 13 轮 spec-review + 实现提交。
- **CI drift（→ Phase 5 收尾）**：
  - CI-1 `cargo fmt` 是 `continue-on-error`(build.yml:24) —— Queued/Phase5
  - CI-2 `cargo clippy` 是 `continue-on-error`(build.yml:29) —— Queued/Phase5
  - CI-3 check job 用 `cargo check` 非 `build --all-targets`(build.yml:26) —— Queued/Phase5
  - CI-4 无 `cargo test` —— Queued/Phase5
  - CI-5 无 `cargo audit` —— Queued/Phase5

---

## 发现汇总表

| ID | 模块 | 维 | 严 | 一句话 | 状态 | 来源 |
|---|---|---|---|---|---|---|
| L-001 | archive/txn | 正确 | **P0** | 提交路径全程无 fsync(标记/索引/父目录),断电可丢索引或标记 | Queued | CR-02 |
| L-002 | manifest | 正确 | **P1** | diff 只遍历 dst,不显式核对"每个源文件存在",靠 count/bytes 兜底(脆弱) | Queued | CR-01/SF-05 |
| L-003 | archive | 静默 | **P1** | 空源(0 真实文件)被记 SHA256-OK 成功并移源 | Queued | SF-01 |
| L-004 | manifest | 正确 | **P1** | --unsafe-no-hash 移源前复核只比 mtime(+UNIX_EPOCH fallback) | Queued | CR-03 |
| L-005 | archive | 正确 | **P1** | 正式路径忽略 folder_stats 枚举/元数据错误 → 容量误判 | Queued | CR-04 |
| L-006 | drive | 静默 | **P1** | min_drive_gb 防呆从未接线 → 可写入错误小盘(U盘) | Queued | CR-06/SF-03 |
| L-007 | verify/cli | 静默 | **P1** | 失败不进退出码:verify bad>0 仍 Ok;批量失败仍 exit 0 | Queued | SF-04/TD-08 |
| L-008 | archive/cli | 类型 | **P1** | no_hash+关测试 的非法组合用 core/cli 两处运行时 guard 守,会漂移 | Queued | TD-04/TC-07 |
| L-009 | archive_test | 静默 | **P1** | 压缩包测试 exit code 1 当非致命警告放行(no_hash 下是唯一内容闸) | Queued | SF-06 |
| L-010 | archive/safety | 正确 | P2 | 跨卷 rename 无 copy+delete 回退,safety 只 warn → 跨卷永久失败 | Queued | CR-05 |
| L-011 | archive | 正确 | P2 | 本盘 catalog 与全局 catalog 两次 append 非原子;恢复只查全局 | Queued | CR-08 |
| L-012 | archive | 静默 | P2 | 移源后写索引失败:catch-all 把"半成品"与"无副作用失败"抹平 | Queued | SF-02 |
| L-013 | verify | 正确 | P2 | size==0 的清单项跳过大小校验 → 真实 0 字节文件被改不报 | Queued | CR-09 |
| L-014 | archive | 正确 | P2 | no_hash 续传 copy_folder 仅比 size → 等长坏内容通过 | Queued | CR-10 |
| L-015 | archive | 静默 | P2 | 隔离坏文件 create_dir_all().ok() 吞错 → 坏文件可能留存被续传跳过 | Queued | CR-11/SF-07 |
| L-016 | drive | 静默 | P2 | scan_mounted 读编号文件失败 Err(_)=>continue 静默丢盘 | Queued | SF-09 |
| L-017 | archive | 静默 | P2 | catalog_has_project().unwrap_or(false):读索引失败当"无重名"→可能覆盖 | Queued | SF-11 |
| L-018 | archive | 静默 | P2 | append_manual 用 ?:台账写失败会中断整轮归档 | Queued | SF-12 |
| L-019 | manifest | 类型 | P2 | Entry.hash:String 用 ""=no_hash;部分哈希清单会静默放行 | Queued | TD-01 |
| L-020 | archive | 类型 | P2 | verify_status 裸字符串字面量,与实际校验脱钩 | Queued | TD-02 |
| L-021 | drive | 类型 | P2 | DriveInfo.sealed 裸 bool,无"仅可写盘"类型约束 | Queued | TD-03 |
| L-022 | txn | 类型 | P2 | PendingTxn 写结构化、读靠 grab_field 字符串抓取,round-trip 不对称 | Queued | TD-05 |
| L-023 | drive/safety | 正确 | P2 | 盘符裸 String 四处重复校验 + 字节切片对多字节首字符 panic | Queued | TD-06/CR-07 |
| L-024 | manifest | 类型 | P2 | Diff.ok 独立 bool 可与 reasons 漂移 | Queued | TD-07 |
| L-025 | config | 类型 | P2 | Config 无加载后校验(三根目录互斥/嵌套、name_prefix) | Queued | TD-09 |
| L-026 | safety | 测试 | P2 | check_paths/is_inside 零测试(关键路径) | Queued | TC-05 |
| L-027 | drive | 测试 | P2 | parse_drive_number/next_drive_number 单调性 零测试 | Queued | TC-06 |
| L-028 | manifest | 静默 | P3 | build 两次读 metadata + 无意义 .ok() | Queued | SF-08 |
| L-029 | safety | 静默 | P3 | folder_stable 吞 walkdir 错误(已注释声明,边角) | Queued | SF-10 |
| L-030 | drive | 静默 | P3 | seal 统计吞错 → 封盘标记可能记 0/0 | Queued | SF-13 |
| L-031 | status/find | 类型 | P3 | status/find/config-show 直接 println! 绕过 Reporter | Queued | TD-10 |
| L-032 | README | 文档 | P2 | 结构图写 engine/ui.rs,实际 reporter.rs + cli/terminal_reporter.rs | Queued | CM-01 |
| L-033 | toml.example | 文档 | P2 | 注释称需传 --source/--target/--archived,实际无此 flag | Queued | CM-02 |
| L-034 | cli.rs | 文档 | P3 | Init doc "设卷标",实现未设卷标 | Queued | CM-03 |
| L-035 | cli.rs | 文档 | P3 | --config doc 含糊 bftool.toml vs config.toml 命名 | Queued | CM-04 |
| L-036 | mod.rs | 文档 | P3 | 文件分工 doc 漏 cruft.rs / archive_test.rs | Queued | CM-05 |
| L-037 | reporter.rs | 文档 | P3 | doc 称 "emoji 前缀",实际 ASCII `[i]/[✓]/[!]/[x]/[>]` | Queued | CM-06 |
| L-038 | README | 文档 | P3 | step1 称 zip 含 2 文件,实际 4(含 README/LICENSE) | Queued | CM-07 |
| L-039 | spec A | 文档 | P3 | 行号引用已漂移(加"实现前快照"banner) | Queued | CM-08 |
| L-040 | manifest | 文档 | P3 | real_files doc 称跳过 reparse point,实际只 follow_links(false) | Queued | CM-09 |
| L-041 | archive | 测试 | P3 | leading_number/leading_digits 零测试 | Queued | TC-08 |
| L-042 | find | 测试 | P3 | find 关键词匹配 零测试 | Queued | TC-09 |
| L-ENV | (环境) | CI/构建 | P2 | 项目可构建性完全绑死重型 MSVC,无"无 MSVC"防呆/gnu 回退/文档 | Queued | 主控 |
| L-SF14 | main.rs | 静默 | P3 | SetConsoleCP let _ 吞错 | **Rejected** | SF-14 |

> 系统性发现 **L-TEST**（P1，伞）：核心数据安全模块(manifest diff、txn 崩溃恢复、verify 四类损坏)**零测试**(TC-01/02/03/04)。不单列修复,而是**每个 P0/P1/P2 修复必须带回归测试**来逐步消化 + L-026/L-027 补关键纯函数测试。

---

## P0 / P1 详细块

### L-001 | P0 | archive.rs + txn.rs | 提交路径无 fsync
- **Broken invariant**：持久性 / 崩溃一致性(设计原则 4)。`txn::PendingTxn::write`(txn.rs:42 `fs::write`)、`write_csv`/`append_drive_catalog`/`append_global_catalog`(archive.rs:496-527 仅 `flush()`)、`txn::clear`(remove_file)全程**无 `sync_all` 也无父目录 fsync**。`fs::rename` 移源(archive.rs:487)的目录项元数据可能先于数据/标记落盘。
- **后果**：断电后可能"源已 rename 移走,但事务标记或索引丢失" → `check_pending_txn` 看不到标记 → 项目静默消失出"待备份"且不在索引(实际不可寻 = 实质数据丢失)。
- **证据**：txn.rs:42；archive.rs:484-529。来源 CR-02(high)。
- **建议测试**：抽 `commit_atomic` 注入可计数 sync trait,断言提交路径对(标记/各索引/各自父目录)都调用 sync 且顺序为 标记→移源→索引→删标记。先断言"父目录被 sync"会失败。
- **修复方向**：提交关键写入后 `File::sync_all`;关键目录用父目录句柄 fsync;封装一个 `durable_write` + `durable_rename`。

### L-002 | P1 | manifest.rs | diff 不显式核对源文件存在
- **Broken invariant**：逐文件双向比对应对称。`diff`(manifest.rs:216-257)只遍历 `dst.entries`;"源有、目标缺"只靠 `count` 与 `total_bytes` 聚合量兜底。当前 rel 唯一 + cruft 对称下能兜住,但属脆弱设计;一旦 cruft 在源/目标侧不对称(目标盘有 `Thumbs.db` 被跳过),count/bytes 可被凑平 → 真实缺失静默放行。reasons 还会把"缺失+多出"误报成"仅多出"。
- **证据**：manifest.rs:216-257。来源 CR-01(med)/SF-05(med)。
- **建议测试**：构造 src={A,B}、dst 仅={A} 的两个 Manifest,断言 `diff(...).ok==false` 且 reasons 含 B;再构造目标侧 cruft 凑平 count/bytes 的对称陷阱,断言仍 `ok==false`(当前会漏报)。
- **修复方向**：`diff` 增加"遍历 src,缺失则 ok=false + 记 rel"的显式环,不只依赖聚合量。
- **注**：可能升 P0(若证明真实可触发的静默漏检)→ 留对抗复审确认。

### L-003 | P1 | archive.rs | 空源被记成功移源
- **Broken invariant**：fail-closed;三重校验任一不符=失败重做。`handle_one` 无 `src.count()==0` 闸;空清单 src/dst 同为 0 → diff 通过、source_changed 通过 → 写 SHA256-OK、file_count=0、移源。"什么都没备份"被记成功且源被移走。
- **证据**：archive.rs:359/444/487;manifest.rs:223-235/267-279。来源 SF-01(high)。
- **建议测试**：空目录(或仅含 cruft)作为项目跑 archive,断言不写索引、不移源、落需人工处理,而非 Done。
- **修复方向**：handle_one 在校验前加 0 真实文件 → Skipped+人工确认。

### L-004 | P1 | manifest.rs | no_hash 复核只比 mtime
- **Broken invariant**：移源前复核源未变(设计原则 4)在 no_hash 下失效。`source_changed` no_hash 分支(manifest.rs:296-300)只比 mtime 字符串;mtime 来自 `modified().unwrap_or(UNIX_EPOCH)`(manifest.rs:133),读取失败时两次都 UNIX_EPOCH → 恒等 → "复制期间内容被改但 mtime 读不到"永远判未变,源照常移走。
- **证据**：manifest.rs:296-300、133。来源 CR-03(high)。
- **建议测试**：两个 Manifest 同 rel/同 size、hash 不同但 mtime 相同 → `source_changed(...,true)` 应 changed==true(当前 false)。
- **修复方向**：no_hash 下复核也纳入 size+mtime 且对 UNIX_EPOCH fallback 视为"不可信→保守判变";或要求 mtime 可读否则 fail-closed。

### L-005 | P1 | archive.rs | 正式路径忽略 folder_stats 错误
- **Broken invariant**：fail-closed / 容量判断正确。`folder_stats` 收集 `enum_errors`/`metadata_errors`(archive.rs:734/738),但正式路径 `size=stats.bytes`(225)直接用于容量判断,**只有 dry-run 才读错误向量**(282-297)。元数据读不到的文件字节不计入 → 容量低估 → 误判"放得下" → 复制到一半撑爆(靠 OS ENOSPC,错误不带"怎么修")。
- **证据**：archive.rs:224-246 vs 282-297;folder_stats 724-742。来源 CR-04(high)。
- **建议测试**：mock folder_stats 返回非空 metadata_errors,断言复制开始前就 Skipped+人工,而非带病进 copy。
- **修复方向**：正式路径与 dry-run 一样在复制前检查并 fail-closed。

### L-006 | P1 | drive.rs | min_drive_gb 防呆未接线
- **Broken invariant**：fail-closed / 绝不写错盘。`config.min_drive_gb`(防误抓 U 盘,注释明说)只在 status.rs:24 被打印;`scan_mounted`/`pick_active`(drive.rs:54-99)从不比较 `total_bytes`。任何带 `本盘信息\本盘编号.txt` 的小介质都会被选为写入目标。
- **证据**：config.rs:25-27/56-58;drive.rs:54-99;消费点仅 status.rs:24。来源 CR-06(high)/SF-03(high)。
- **建议测试**：fake 两盘(8GB+带 id、500GB),min_drive_gb=200,断言 scan/pick 排除小盘(或 warn 不自动选)。
- **修复方向**：scan_mounted/pick_active 加 `total_bytes >= min_drive_gb` 过滤,低于阈值 warn 并排除。

### L-007 | P1 | verify.rs + cli | 失败不进退出码
- **Broken invariant**：fail-closed;自动化只能可靠消费退出码。`verify::run` 累计 bad/extra 后无条件 `Ok(())`(verify.rs:179);archive 无可用盘/待备份不存在/全项目失败 都 `Ok(())`(archive.rs:60/70/120/176-190)。调度脚本据 exit 0 误判"备份成功"。
- **证据**：verify.rs:165-180;archive.rs:60-190;main.rs:22。来源 SF-04(high)/TD-08(high)。
- **建议测试**：无盘 / 全项目失败 两场景断言进程 exit code 非 0;verify 对含 1 个损坏文件的树返回 bad==1 的结构化结果。
- **修复方向**：core 返回结构化结果(`VerifyReport`/run 聚合成功失败数),CLI 映射 bad>0 或 失败>0 → 非零退出。

### L-008 | P1 | archive.rs + cli.rs | no_hash 非法组合双处 guard 易漂移
- **Broken invariant**：唯一阻止"无任何完整性校验的备份"的边界,被 core(archive.rs:34-44)与 cli(cli.rs:138-161)两处等价但重复的运行时 guard 守 → 未来一处改动静默绕过。根因是 `Options` 允许该非法组合被构造。
- **证据**：archive.rs:32-44;cli.rs:138-161。来源 TD-04(high)/TC-07(med)。
- **建议测试**：`Options::new(no_hash+关测试)` 返回 Err;GUI 直构路径同样被挡。
- **修复方向**：core 给 `Options` 单一可失败构造函数 / `VerifyPolicy` enum(无"两者皆无"变体),core+cli+GUI 共用,删重复 bail。

### L-009 | P1 | archive_test.rs | 压缩包测试 exit 1 fail-open
- **Broken invariant**：fail-closed;校验器"警告"不应静默等同"通过",尤其它是 no_hash 下唯一内容闸。`code==1 => NonFatalWarn`(archive_test.rs:196)只 warn 不置 `ok=false`(270-276),项目照常提交。7-Zip/WinRAR 的 1 含"部分文件无法读取"。
- **证据**：archive_test.rs:196、270-276。来源 SF-06(med)。
- **建议测试**：mock invoke 返回 code=1,断言 hash 模式升级为显式 Action 并记录;no_hash 模式视为不通过/要人工确认,不静默 Done。
- **修复方向**：no_hash 下 code=1 不放行;hash 模式至少记录到清单。

---

## 修复顺序（Phase 3，待 MSVC）

P0 → P1 → P2 → (P3 文档批量)。每条 TDD 红-绿 + 门禁 + 更新本 ledger 状态(Fixed→对抗复审 Verified)。

候选优先：**L-001(fsync)、L-002(diff 显式核对)、L-007(退出码)、L-006(min_drive_gb)、L-003(空源)、L-008(Options guard 收口)** —— 直接关系"假成功 / 校验绕过 / 数据可寻"核心不变量。
