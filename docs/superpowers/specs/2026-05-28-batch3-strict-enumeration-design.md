# Spec A：Batch 3 —— manifest 枚举严格化 + cruft 默认排除

* 日期：2026-05-28
* 状态：批准（user-approved 2026-05-28）
* 上游路线图：原 Batch 3「枚举错误默认失败 + --allow-skip-errors + skipped-files.csv」
* 关联 spec：[Spec B：Batch 3.5 —— 压缩包专项完整性测试](./2026-05-28-batch3.5-archive-integrity-test-design.md)

---

## 1. 一句话

任何枚举 / metadata / 读文件失败默认让当前项目失败、跳到下一项目；同时把 OS 注入的杂文件（Thumbs.db / desktop.ini / .DS_Store / `._*` / $RECYCLE.BIN 等）从源头排除，根本不进 manifest。

## 2. 动机

现状（Batch 1b 落成后）：
- `crates/bftool-core/src/engine/manifest.rs::real_files` 对 walkdir 错误是 `reporter.warn + continue`。结果：权限拒绝、路径过长、IO 抖动等情况下文件被悄悄漏掉，三重校验通不过却**看不出哪里缺**，或者更糟，看似成功但少备份了文件。
- 同时，Windows / macOS 会在用户的目录里自动塞 `Thumbs.db` `desktop.ini` `.DS_Store` 这类元数据文件，它们不是项目内容。如果上面那条改成严格模式，**它们一被 antivirus 锁或者权限不足就会黄掉整项目** —— 这是新手最容易踩的坑。

目标：
- 默认就是「完整安全」的路径，新手永远走在这条路上
- 不让 OS 杂文件成为新手的"项目莫名其妙失败"原因
- **不**给新手提供放宽选项（路线图原本要做的 `--allow-skip-errors` 不实现），杜绝误开危险模式

## 3. 设计决策（已对齐）

| # | 决策 | 选项 |
|---|---|---|
| D1 | 作用范围 | 只在 archive 流程；verify 始终严格 |
| D2 | OS 杂文件 | 默认 hardcoded 排除，不可配置（独立于错误处理） |
| D3 | 是否提供放宽 flag | **不提供** `--allow-skip-errors`、不提供双确认 flag |
| D4 | 错误传播 | 收集后一次输出（不 first-error 立即停） |
| D5 | skipped-files.csv | **不实现**（用户场景单压缩包用不上） |

D3 / D5 即「简化」相对路线图原 Batch 3 的具体内容。

## 4. 架构改动

### 4.1 新增 `crates/bftool-core/src/engine/cruft.rs`

包含 cruft 名单 + **共享的 cruft-aware walker**。**五处** `archive::handle_one` 会走的目录遍历都用同一个 walker —— 稳定性检测 / manifest / copy / size 统计 / verify —— 保证 cruft 在所有路径上的处理一致：不卡稳定性检测、不进 manifest、**不复制到机械盘**、不算入 folder_stats、verify 也不报"清单外多余"。

```rust
//! OS 注入的杂文件 / 目录，默认从 manifest、复制、容量统计全部排除。
//! 这些不是项目内容，无须用户配置。
use anyhow::Result;
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

/// 文件名精确匹配（不分大小写）
pub const CRUFT_FILES: &[&str] = &[
    "Thumbs.db",       // Windows 缩略图缓存
    "desktop.ini",     // Windows 文件夹元数据
    ".DS_Store",       // macOS Finder 元数据
    "ehthumbs.db",     // Windows Media Center 缩略图
    "ehthumbs_vista.db",
];

/// 目录名精确匹配（不分大小写）—— 整个目录跳过，不递归进去
pub const CRUFT_DIRS: &[&str] = &[
    "$RECYCLE.BIN",
    "System Volume Information",
    "found.000",
    "lost+found",
    ".Trashes",
    "Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
];

/// 前缀模式：以 "._" 开头（macOS resource fork on non-HFS volumes）
pub fn is_cruft_file(name: &str) -> bool {
    if name.starts_with("._") { return true; }
    CRUFT_FILES.iter().any(|c| c.eq_ignore_ascii_case(name))
}

pub fn is_cruft_dir(name: &str) -> bool {
    CRUFT_DIRS.iter().any(|c| c.eq_ignore_ascii_case(name))
}

/// 从 manifest 的 `Rel` 字段提取叶子名（**第四轮 P3 修复**）。
///
/// manifest 的 `Rel` 是 PowerShell 旧版兼容格式，**永远用 `\` 分隔**（哪怕在 Linux 跑测试）：
/// `crates/bftool-core/src/engine/manifest.rs:19`。直接用 `Path::new(rel).file_name()` 在
/// 非 Windows 平台会把整串 "foo\Thumbs.db" 当成叶子名，cruft 判断漏判。
///
/// 这里手动按 `\` 和 `/` 都切，取最后一段。
pub fn leaf_name_from_rel(rel: &str) -> &str {
    rel.rsplit(|c| c == '\\' || c == '/').next().unwrap_or(rel)
}

/// 共享的 cruft-aware 遍历器。
///
/// 调用方拿到的 `DirEntry` 序列已经过滤掉：
/// - cruft 目录（递归整段跳过；root 自身豁免，depth==0 不当 cruft）
/// - cruft 文件名（精确 + `._` 前缀）
/// - 不跟 symlink / junction（follow_links=false）
///
/// **不**屏蔽 walkdir 自身的错误 —— 错误透传，由调用方决定怎么处理
/// (manifest::real_files: 收集后 bail；archive_test::test_folder: 同；
/// archive::copy_folder: 收集后 bail；verify: 计入 bad)。
pub fn walk(root: &Path) -> impl Iterator<Item = walkdir::Result<DirEntry>> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // depth==0 是 root 自身，不当 cruft
            if e.depth() == 0 { return true; }
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() {
                !is_cruft_dir(&name)
            } else {
                !is_cruft_file(&name)
            }
        })
}
```

> `filter_entry` 在 walkdir 里的行为：对目录返回 `false` 时整段跳过递归；对文件返回 `false` 时仅跳过该文件。完全契合我们的需求。**注意**：`filter_entry` 对返回 `Err` 的 entry 不调用 predicate，错误会直接 yield 出来 —— 这正是我们要的"错误透传"语义。

### 4.2 改造 `manifest::real_files`

**签名变化**：`Vec<PathBuf>` → `Result<Vec<PathBuf>>`。cruft 过滤靠共享 walker，本函数只剩"收集错误 + 收集文件"的薄逻辑：

```rust
fn real_files(root: &Path, reporter: &dyn Reporter) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut errors: Vec<String> = Vec::new();  // 收集后一次报告（D4）

    for entry in cruft::walk(root) {
        match entry {
            Ok(e) if e.file_type().is_file() => out.push(e.into_path()),
            Ok(_) => continue,  // 目录、symlink 等，本步不收
            Err(err) => errors.push(format!("{}", err)),
        }
    }

    if !errors.is_empty() {
        for e in &errors {
            reporter.error(&format!("枚举失败：{}", e));
        }
        anyhow::bail!(
            "枚举本项目时遇到 {} 个错误（详见上方）→ 本项目跳过，不影响后续项目；下次运行会重做。",
            errors.len()
        );
    }
    Ok(out)
}
```

### 4.2b 改造 `archive::copy_folder`（**新增改动，原 spec 漏了**）

P1.1 修复：cruft 在复制阶段也必须排除，否则虽不入 manifest 但物理存在于备份盘上、verify 会报「清单外多余」。改用共享 walker：

```rust
fn copy_folder(src: &Path, dst: &Path, reporter: &dyn Reporter) -> Result<()> {
    fs::create_dir_all(dst).ok();
    let mut errors: Vec<String> = Vec::new();
    for entry in cruft::walk(src) {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => { errors.push(format!("{}", err)); continue; }
        };
        let path = entry.path();
        let rel = path.strip_prefix(src)?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).ok();
        } else if entry.file_type().is_file() {
            // 已存在且大小一致 → 视为已传，跳过（与 robocopy 默认行为一致）
            if let (Ok(meta_src), Ok(meta_dst)) = (path.metadata(), target.metadata()) {
                if meta_src.len() == meta_dst.len() { continue; }
            }
            if let Some(p) = target.parent() { fs::create_dir_all(p).ok(); }
            fs::copy(path, &target)
                .with_context(|| format!("复制失败：{} → {}", path.display(), target.display()))?;
        }
    }
    if !errors.is_empty() {
        for e in &errors { reporter.error(&format!("复制阶段枚举源失败：{}", e)); }
        anyhow::bail!("复制阶段枚举源失败 {} 项 → 本项目跳过。", errors.len());
    }
    Ok(())
}
```

### 4.2c 改造 `archive::folder_size` → 新增 `folder_stats`（**新增改动**）

容量预估不能算 cruft，否则跟 manifest / copy 不一致。同时按 user review (P3-dry-run)，dry-run 既要报 GB 也要报"X 个文件"，所以把单一字节数改为双字段：

```rust
#[derive(Debug, Clone, Default)]
pub struct FolderStats {
    pub files: u64,
    pub bytes: u64,
    /// walkdir 枚举错误描述（第四轮 P2 修复）。
    /// 容量判断时忽略（best-effort），但 dry-run 输出要向用户暴露：
    /// "你看到的'X 个文件'可能不完整，请先解决环境问题再正式跑"。
    pub enum_errors: Vec<String>,
}

fn folder_stats(p: &Path) -> FolderStats {
    let mut s = FolderStats::default();
    for entry in cruft::walk(p) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                s.files += 1;
                if let Ok(m) = e.metadata() {
                    s.bytes += m.len();
                }
            }
            Ok(_) => continue,
            Err(err) => s.enum_errors.push(format!("{}", err)),
        }
    }
    s
}
```

`handle_one` 的 dry-run 分支改为（**第四轮 P2 修复**：dry-run 报告统计可能不完整）：

```rust
let stats = folder_stats(proj_path);
if opts.dry_run {
    reporter.info(&format!(
        "[演练] 将归档 {} ({} 个文件, {:.2}GB) → {}",
        name, stats.files, stats.bytes as f64 / 1024.0 / 1024.0 / 1024.0, drive.id
    ));
    if !stats.enum_errors.is_empty() {
        for e in &stats.enum_errors {
            reporter.warn(&format!("[演练] 枚举失败：{}", e));
        }
        reporter.warn(&format!(
            "[演练] {} 项目的统计可能不完整 ({} 个枚举错误)。正式归档会因为同样的错误失败 \
             —— 请先解决环境问题（权限拒绝？路径过长？被 AV 锁住？）再去掉 --dry-run 跑。",
            name, stats.enum_errors.len()
        ));
    }
    return Ok(HandleOutcome::Done);
}
```

为什么 folder_stats 仍 swallow（**不** bail）：
1. 它服务于"容量判断 + dry-run 摘要"两个场景
2. 容量判断里就算少算几个文件,正式跑时 `manifest::real_files` 会精确枚举并 bail —— 容量这层 best-effort 是 OK 的
3. dry-run 路径必须**主动暴露**这些错误（user-review-P2 修复:之前是悄悄 swallow,违反 Spec A 总原则）—— 上面 if 分支的 warn 就是
4. 让 folder_stats 直接 bail 会让单一权限拒绝在容量判断阶段就把项目挡掉,看不到 manifest 阶段的多文件错误汇总

### 4.2d 改造 `safety::folder_stable`（**user-review-第三轮 P1 修复**）

`safety::folder_stable` 是 `archive::handle_one` 的**第一关**（在 manifest / copy / stats 之前）。
当前实现裸跑 `WalkDir`：

```rust
// crates/bftool-core/src/engine/safety.rs:101 当前实现
for entry in WalkDir::new(root).follow_links(false) {
    let Ok(e) = entry else { continue };
    if !e.file_type().is_file() { continue; }
    // 检查 mtime > cutoff、试图 File::open
}
```

Thumbs.db 被 antivirus 持续触碰是 Windows 上的常态，会让稳定性检测报"最近被修改"或"被占用"，**项目根本走不到 manifest 阶段就被早退**。这正好违反 Spec A "OS 杂文件不影响项目"的目标。

修复：

```rust
pub fn folder_stable(root: &Path, minutes: u64) -> StableCheck {
    let cutoff = SystemTime::now() - Duration::from_secs(minutes * 60);
    for entry in cruft::walk(root) {  // 改用共享 walker（自动跳 cruft）
        let Ok(e) = entry else {
            // 稳定性检测对 walkdir 错误**保持原 swallow 语义**：
            // 真实枚举错误会在后续 manifest::real_files 阶段被收集并 bail。
            // 让稳定性检测也 bail 会让单个"权限拒绝"在第一关就把项目挡掉，
            // 用户看不到 manifest 阶段更详细的多文件错误汇总。
            continue;
        };
        if !e.file_type().is_file() { continue; }
        if let Ok(meta) = e.metadata() {
            if let Ok(mt) = meta.modified() {
                if mt > cutoff {
                    return StableCheck { stable: false, reason: format!(
                        "最近被修改：{}", e.file_name().to_string_lossy()
                    )};
                }
            }
        }
        if let Err(err) = File::open(e.path()) {
            return StableCheck { stable: false, reason: format!(
                "被占用/无法读取：{}（{}）", e.file_name().to_string_lossy(), err
            )};
        }
    }
    StableCheck { stable: true, reason: String::new() }
}
```

副效应：被 antivirus 锁的 Thumbs.db / .DS_Store 不再触发"未稳定"。

### 4.3 `manifest::build` 与 metadata 错误

`build` 内部已经是 `.with_context(...)?` 立即抛错 —— 这本来就是 strict 行为。但要按 D4 调整为「收集所有 metadata 错误后一次报告」：

```rust
pub fn build(root: &Path, opts: ManifestOpts, reporter: &dyn Reporter) -> Result<Manifest> {
    let files = real_files(root, reporter)?;  // 枚举错误已在这一步收集后 bail
    // ...
    let mut metadata_errors: Vec<String> = Vec::new();
    let mut hash_errors: Vec<String> = Vec::new();
    let mut entries = Vec::with_capacity(files.len());

    for f in &files {
        let meta = match f.metadata() {
            Ok(m) => m,
            Err(e) => {
                metadata_errors.push(format!("{}: {}", f.display(), e));
                continue;
            }
        };
        let size = meta.len();
        let mtime = system_time_to_rfc3339(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        let rel = path_relative(&base, f);

        let hash = if opts.no_hash {
            String::new()
        } else {
            match sha256_hex(f) {
                Ok(h) => h,
                Err(e) => {
                    hash_errors.push(format!("{}: {}", f.display(), e));
                    continue;
                }
            }
        };
        bar.inc(size);
        entries.push(Entry { rel, size, hash, mtime });
    }
    bar.finish();

    if !metadata_errors.is_empty() || !hash_errors.is_empty() {
        for e in &metadata_errors { reporter.error(&format!("读元数据失败：{}", e)); }
        for e in &hash_errors { reporter.error(&format!("读文件内容失败：{}", e)); }
        anyhow::bail!(
            "本项目有 {} 个元数据失败、{} 个内容读取失败 → 本项目跳过，下次重做。",
            metadata_errors.len(), hash_errors.len()
        );
    }
    Ok(Manifest { entries })
}
```

### 4.4 verify.rs：两处改（**user-review 三轮累计**）

verify 有**两条**遍历路径，要分别处理：

**(a) 清单外多余文件扫描**（已在 verify.rs:130 附近）—— P2.2 + 三轮 P2-A 修复：用 `cruft::walk`，walkdir 错误计入 bad：

```rust
for entry in cruft::walk(&proj_dir) {
    let entry = match entry {
        Ok(e) => e,
        Err(err) => {
            reporter.error(&format!("  枚举失败: {}", err));
            bad += 1;
            continue;
        }
    };
    // ... 余下逻辑不变（检查是否在 expected 里，不在则 extra++）
}
```

副效应：verify 不会再把 cruft 文件当"清单外多余"warn。

**(b) 旧 manifest 逐行 check**（verify.rs:87 附近）—— **第三轮 P2 修复**：

verify 第一条路是读 manifest 里的每个 `Rel` 去 check 文件存在 / 大小 / 哈希。如果旧 manifest **（Batch 3 之前归档的项目）**里登记过 `Thumbs.db`，新代码会让它变成"缺失"（用户手动删了）或"读取失败"（被锁）。

修复：读 manifest 行时，如果 `Rel` 的叶子名命中 cruft 规则，**跳过该条目** —— 既不进 expected、也不 check。这相当于把"旧清单里的 cruft 条目"视为"工具不再关心的遗留"，跟新清单不再登记 cruft 行为一致。

```rust
for rec in rdr.records().flatten() {
    let rel = rec.get(i_rel).unwrap_or("").to_string();
    // 第四轮 P3 修复：用 cruft::leaf_name_from_rel 而不是 Path::file_name
    // —— manifest Rel 永远 \\ 分隔，跨平台 Path::file_name 会漏判
    let leaf = cruft::leaf_name_from_rel(&rel);
    if cruft::is_cruft_file(leaf) {
        // legacy cruft：旧清单有这条，但 Batch 3 后我们不再关心 cruft
        // → 不加入 expected，不 check 存在/大小/哈希
        // → 用户手动删 Thumbs.db 不会被报"缺失"；被 AV 锁也不会读取失败
        continue;
    }
    // ... 余下逻辑不变（expected.insert + check）
}
```

> 不对"旧 cruft 目录条目"做处理：cruft 目录不会出现在旧 manifest 里，因为 PowerShell 旧版和 Rust 之前的版本都只记录文件,不记目录。如果以后真出现这种边角情况,用户跑 verify 会看到"缺失"warn，这是可见信号，不会引起静默错误。

### 4.5 不改的地方

- `crates/bftool-cli/src/cli.rs`：**不增加任何 flag**（D3）
- `crates/bftool-core/src/engine/archive.rs::handle_one`：**外层 match 结构不变**，已有的"项目级 try/catch"自然处理 `real_files` / `copy_folder` 冒泡上来的错误。**仅有的内部改动**：dry-run 分支输出文字加"X 个文件"（见 §4.2c）。`copy_folder` 和原 `folder_size`（现 `folder_stats`）两个 helper 内部用共享 walker。

## 5. 错误处理与人机交互

| 场景 | 当前行为 | Batch 3 后行为 |
|---|---|---|
| 文件夹里有 Thumbs.db | 进 manifest，可能因 antivirus 锁住导致整项目失败 | 直接不枚举，不存在 |
| `$RECYCLE.BIN` 子目录 | 递归进去（一般空，但浪费时间） | 整目录跳过 |
| 文件夹里有 5 个 `._foo` | 都进 manifest，传到机械盘上一堆资源叉 | 全部不枚举 |
| walkdir 遇到 1 个权限拒绝目录 | warn 一条继续，文件被漏掉 | 收集所有 walkdir 错误一次报告 + 项目失败 |
| 5 个文件 metadata 读不出 | metadata 第一个 `?` 立刻 bail，只看到第一个 | 5 个都尝试，一次报告全部 |
| 1 个文件 hash 中读出错 | hash 函数 `?` bail，project 失败 | 收集本文件错误继续算下一个；最后一次报告 |

## 6. 测试边界

按 Batch 8（CI 加固）会写 Rust 集成测试，本次 spec 只锁清单：

1. `cruft::is_cruft_file`：精确匹配 Thumbs.db / desktop.ini / .DS_Store；大小写不敏感；前缀 `._` 命中
2. `cruft::is_cruft_dir`：精确匹配；不接受 `$RECYCLE.BIN/sub` 这种子路径（只看叶子名）
3. `real_files`：含 cruft 的临时目录 → cruft 不出现在结果里
4. `real_files`：含一个不可读子目录 → 返回 `Err`，错误信息提到该子目录
5. `manifest::build`：3 个文件中 1 个 metadata 失败 → 错误信息列出失败的那 1 个的路径，不混淆其它 2 个
6. 集成测试：`bftool archive --dry-run` 在含 cruft 的源目录上，输出"X 个文件"的统计**不**包含 cruft
7. **`safety::folder_stable`（第三轮 P1）**：(a) 含一个 5 分钟前刚改的 Thumbs.db + 一个 1 小时前的真实项目文件 → 返回 stable=true（cruft 不算入稳定性）；(b) 含一个被独占锁住的 Thumbs.db → stable=true
8. **`verify` legacy cruft（第三轮 P2-verify + 第四轮 P3）**：
   - (a) 手写一份旧 manifest 含 `Thumbs.db` 条目 + 盘上有 Thumbs.db → verify summary 报 0 缺失
   - (b) 同上但 Thumbs.db 已被用户删 → 同样 0 缺失
   - (c) **跨平台**：旧 manifest 含 `子目录\\Thumbs.db` 条目（注意 `\\` 分隔，模拟 Windows 写入的旧清单）→ 不管在 Windows 还是 Linux 跑测试,`cruft::leaf_name_from_rel` 都正确取出 `Thumbs.db`,verify 0 缺失
9. **第四轮 P2 `folder_stats` 错误暴露**：含一个不可读子目录的源目录 → `folder_stats` 返回 `enum_errors` 非空；`bftool archive --dry-run` 输出含 `[演练] 枚举失败：...` 和"统计可能不完整"warn
10. **第四轮 P3 `cruft::leaf_name_from_rel`** 单元测试:
    - `"Thumbs.db"` → `"Thumbs.db"`
    - `"foo\\Thumbs.db"` → `"Thumbs.db"`
    - `"a/b/Thumbs.db"` → `"Thumbs.db"`
    - `"a/b\\Thumbs.db"` → `"Thumbs.db"`(混用)
    - `""` → `""`

## 7. 不做的事（明确边界）

- 不做 `--allow-skip-errors`、不做 `--strict-enumeration` 之类 flag（D3）
- 不做 `skipped-files.csv`（D5，因为没有 skip 的概念了）
- 不做用户可配置的 `extra_excludes = [...]`（D2 已锁 hardcoded；以后真有人需要再说）
- ~~不动 `verify.rs`~~ —— **本节作废**。原本意是"verify 仍是报告型语义"，但 §4.4 已确认要改 verify 的实现细节（cruft::walk + walkdir 错误计 bad）。**verify 的「报告型」整体语义不变，但实现要按 §4.4 改**。

## 8. 影响范围

| 文件 | 改动 |
|---|---|
| `crates/bftool-core/src/engine/cruft.rs` | 新建：名单 + 共享 `walk()` + 跨平台 `leaf_name_from_rel(rel)`（第四轮 P3） |
| `crates/bftool-core/src/engine/mod.rs` | `pub mod cruft;` |
| `crates/bftool-core/src/engine/manifest.rs` | `real_files` 改签名 + 用 `cruft::walk` + 收集错误；`build` 改 metadata/hash 错误收集语义 |
| `crates/bftool-core/src/engine/archive.rs` | `copy_folder` 改用 `cruft::walk` + 错误收集后 bail + 签名加 `reporter: &dyn Reporter`；新增 `FolderStats { files, bytes, enum_errors }` 结构和 `folder_stats()` 函数（取代 `folder_size`，**含 enum_errors 暴露**——第四轮 P2）；`handle_one` 的 dry-run 分支报告"X 个文件 Y GB"，**有枚举错误时输出 warn 提示统计可能不完整**（第四轮 P2） |
| `crates/bftool-core/src/engine/safety.rs` | `folder_stable` 改用 `cruft::walk`（第三轮 P1） |
| `crates/bftool-core/src/engine/verify.rs` | 两处改：(a) 扫"清单外多余"那段改用 `cruft::walk` + walkdir 错误计入 bad；(b) 读旧 manifest 时跳过 legacy cruft 条目（第三轮 P2-verify） |
| `README.md` | 「设计原则」一节加一条「OS 杂文件自动排除」；FAQ 加一条「我看 manifest 文件数少了几个，怎么回事」→「Thumbs.db 等已自动排除」 |

CLI 不变、Cargo.toml 不变、CI 不变。

## 9. 向后兼容

- 已有备份盘的 manifest 不重写；既有 SHA256 校验清单不变
- 老的 cruft 文件（已经备份过的 Thumbs.db）仍在盘里：
  - §4.4 改动后 verify 走 `cruft::walk` → **不**会把它们当作"清单外多余"报
  - 它们物理上还在盘上，但不再触发 verify warn，也不影响 archive 流程
  - 用户想清理可以人工删；工具不主动动
- 用户不需要任何迁移动作

## 10. 一句话给新手

> Thumbs.db 这类 OS 杂文件自动不备份；任何读文件出错的项目会被跳过并写在「需人工处理.txt」里——绝不偷偷漏掉文件然后假装成功。
