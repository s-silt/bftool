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

```rust
//! OS 注入的杂文件 / 目录，默认从 manifest 排除。
//! 这些不是项目内容，无须用户配置。

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
```

### 4.2 改造 `manifest::real_files`

**签名变化**：`Vec<PathBuf>` → `Result<Vec<PathBuf>>`

```rust
fn real_files(root: &Path, reporter: &dyn Reporter) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut errors: Vec<String> = Vec::new();  // 收集后一次报告（D4）
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();

    loop {
        let entry = match walker.next() {
            None => break,
            Some(Ok(e)) => e,
            Some(Err(err)) => {
                errors.push(format!("{}", err));
                continue;  // 继续走完，最后一次性 bail
            }
        };

        let name = entry.file_name().to_string_lossy().to_string();

        if entry.file_type().is_dir() {
            // 保护：root 本身不应被当 cruft（用户的 ready_root 不可能是 $RECYCLE.BIN，但严谨起见排除 depth==0）
            if entry.depth() > 0 && cruft::is_cruft_dir(&name) {
                walker.skip_current_dir();
                continue;
            }
        } else if entry.file_type().is_file() {
            if cruft::is_cruft_file(&name) {
                continue;
            }
            out.push(entry.into_path());
        }
        // 其它（symlink、reparse point）：walkdir follow_links=false 已经不递归，跳过即可
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

### 4.4 不改的地方

- `crates/bftool-cli/src/cli.rs`：**不增加任何 flag**（D3）
- `crates/bftool-core/src/engine/verify.rs`：**不动**，仍是「扫一遍出报告」语义（D1）
- `crates/bftool-core/src/engine/archive.rs`：**不动**，已有的 `handle_one` 外层 `match` 自然处理冒泡上来的错误（写 `需人工处理.txt` + 跳到下一项目）

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

## 7. 不做的事（明确边界）

- 不做 `--allow-skip-errors`、不做 `--strict-enumeration` 之类 flag（D3）
- 不做 `skipped-files.csv`（D5，因为没有 skip 的概念了）
- 不做用户可配置的 `extra_excludes = [...]`（D2 已锁 hardcoded；以后真有人需要再说）
- 不动 `verify.rs`（D1）

## 8. 影响范围

| 文件 | 改动 |
|---|---|
| `crates/bftool-core/src/engine/cruft.rs` | 新建 |
| `crates/bftool-core/src/engine/mod.rs` | `pub mod cruft;` |
| `crates/bftool-core/src/engine/manifest.rs` | `real_files` 改签名 + cruft 跳过 + 收集错误；`build` 改 metadata/hash 错误收集语义 |
| `README.md` | 「设计原则」一节加一条「OS 杂文件自动排除」；FAQ 加一条「我看 manifest 文件数少了几个，怎么回事」→「Thumbs.db 等已自动排除」 |

CLI 不变、verify 不变、archive 不变、Cargo.toml 不变、CI 不变。

## 9. 向后兼容

- 已有备份盘的 manifest 不重写；既有 SHA256 校验清单不变
- 老的 cruft 文件（已经备份过的 Thumbs.db）仍在盘里，下次 `verify` 会报「多余(清单外)」warn —— 这没问题，跟其它「之前漏算的杂项」一样的处理
- 用户不需要任何迁移动作

## 10. 一句话给新手

> Thumbs.db 这类 OS 杂文件自动不备份；任何读文件出错的项目会被跳过并写在「需人工处理.txt」里——绝不偷偷漏掉文件然后假装成功。
