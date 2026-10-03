# bftool — 命令行版 & 进阶说明

> 新手用图形界面看 [README.md](README.md) 就够了。本文是**命令行版（`bftool.exe`）完整流程**、配置详解、设计原理、盘内结构、迁移与构建等进阶内容。
> CLI 与 GUI **共用同一套 core 引擎和配置**，行为完全一致，按你习惯任选其一。

---

## 设计原则

1. **绝不删源**。校验通过后，源被**移动**到「已备份」目录，永远等着你自己决定删不删。
2. **不镜像**。从不使用 `robocopy /MIR` 这种"源删了就把备份也删了"的危险操作。
3. **每个项目独立提交**。一个项目损坏不影响其它的恢复。
4. **三重校验**。文件数 + 总字节 + 每文件 SHA256，任意一项不符 = 失败重做。
5. **每盘自洽**。盘里自带索引、校验清单、日志、README，离线也能自我说明。
6. **事务式**。"写标记 → 写清单/索引 → 移动源 → 删标记"（索引先于移源落盘）；宁可漏写索引（可人工补），也不要"写了索引却没真正归档"。
7. **新手友好**。错误信息一定带"具体怎么修"；默认值偏保守；不传任何参数 `bftool` 会给你看当前状态 + 可用命令。
8. **OS 杂文件自动排除**。`Thumbs.db` / `desktop.ini` / `.DS_Store` / `$RECYCLE.BIN` / `System Volume Information` 等系统注入文件**不进 manifest、不复制到机械盘、verify 也不当"清单外多余"报**。这些不是项目内容，排除它们让新手不会因为 antivirus 锁 Thumbs.db 莫名其妙整项目失败。

> **备份不等于"复制完了"**。借自 [rsure](https://github.com/d3zd3z/rsure) 的提醒：*backups aren't useful unless you've tested them.* 跑 `bftool verify <盘符>` 才算"已验证的备份"，建议每 6–12 个月一次。

---

## 命令行版（CLI）快速上手

### 1. 配置（30 秒）

把 `bftool.toml.example` 重命名为 `bftool.toml`，用记事本打开，把三个根目录改成你自己的：

```toml
ready_root    = "D:\\资料库\\待备份"
archived_root = "D:\\资料库\\已备份"
system_root   = "D:\\资料库\\备份系统"
```

> CLI 默认读当前目录的 `bftool.toml`；GUI 默认读 `%APPDATA%\bftool\config.toml`。两者格式相同，可互相复制。

### 2. 看一眼状态

在 `bftool.exe` 所在目录打开 PowerShell 或 cmd，跑：

```cmd
bftool
```

会显示当前的源目录、检测到的备份盘、待归档项目数、可用子命令。

### 3. 初始化目标盘

插上一块机械硬盘作备份盘（NTFS 格式；**不要求空盘**），假设盘符是 `E:`：

```cmd
bftool init E
```

工具会：

- 自动取下一个编号（`备份1`、`备份2` …）
- 在盘里建好 `\项目\`、`\本盘信息\`（含日志、校验清单子目录）
- 写本盘说明文件

### 4. 演练 → 正式备份

把做完的项目文件夹挪到 `待备份\` 下，然后：

```cmd
bftool archive --dry-run    # 演练：只列出会做什么，不真正动文件
bftool archive              # 正式开跑
```

每个项目会经过：稳定性检测 → 体积检查 → 完整复制 → 三重校验 → 源在复制中是否变过 → 事务式提交。

中途 `Ctrl+C` 可以随时中断，下次跑 `bftool archive` 会从断点继续。

### 5. 盘写满了

工具会自动**封盘**（写 `已封盘.txt`）并提示。取下这块盘贴好标签离线收好，插上下一块目标盘，再 `bftool init <新盘符>` → `bftool archive`。

### 6. 复查（防坏道）

```cmd
bftool verify E
```

会重算每个项目的 SHA256 跟清单比对，报告损坏/缺失/多余文件。

### 7. 找以前的项目

```cmd
bftool find 关键词
```

会在全局索引里搜，告诉你它在哪块「备份N」、盘内路径。

---

## 子命令速查

| 命令 | 作用 |
|---|---|
| `bftool` | 显示当前状态 + 可用子命令 |
| `bftool archive` | 归档：处理 待备份 下所有就绪项目 |
| `bftool archive --dry-run` | 演练（不复制、不写索引、不移动源） |
| `bftool archive --unsafe-no-hash --i-understand-this-can-miss-bitrot` | **危险**：跳过 SHA256 内容校验，挡不住静默损坏（比特腐烂）。必须两个开关同时传才生效，单独 `--unsafe-no-hash` 会被拒绝。仅适合海量素材类、且接受静默损坏不可见的场景；**不可再生的资料请保持完整 SHA256**。开启此项要求机器装了 7-Zip/WinRAR/Bandizip 任一（让压缩包测试兜底）；同时关压缩包测试 → 直接拒绝。 |
| `bftool archive --no-test-archives` | 关掉压缩包内部结构测试（默认开启）。SHA256 字节级校验仍在。与 `--unsafe-no-hash` 互斥（两者同关 ≈ 没在校验）。 |
| `bftool archive --limit 1` | 本次只处理一个项目 |
| `bftool init <盘符>` | 初始化目标盘为下一个「备份N」（不要求空盘；非空仅警告） |
| `bftool init <盘符> --force` | 跳过硬闸（系统盘/资料库盘）；非空默认允许无需 force；风险自负 |
| `bftool verify [盘符]` | 复查指定盘 |
| `bftool find <关键词>` | 在全局索引查项目位置 |
| `bftool drives` | 列出已识别的备份盘 |
| `bftool config-show` | 显示当前生效的配置 |

跑 `bftool <命令> --help` 看完整选项。

---

## 装了 7-Zip 自动测压缩包

如果你的机器装了 [7-Zip](https://7-zip.org)（或 WinRAR / Bandizip），bftool 会在归档前后各跑一次 `t` 测试压缩包内部结构 —— 确保**字节没变**（SHA256 兜底）**+ 结构也没坏**（archive test 兜底）。没装也能跑，只是少一道压缩包专项保险，启动时会给一行 warn。

**与 `--unsafe-no-hash` 互斥**：如果你同时关 SHA256 又关 archive test（或者机器上一个压缩工具都没装），bftool 会拒绝运行 —— 那种状态只剩文件数 + 大小 + 修改时间，等价于"没在做完整性校验"。

需要彻底关压缩包测试？显式传 `--no-test-archives`，或在 `bftool.toml` 里写 `test_archives = false`。

---

## 每块盘里有什么

```
备份3 (E:)
├─ 项目\               原样存放，可直接复制恢复
│   ├─ 001某某项目\
│   └─ 002另一个项目\
└─ 本盘信息\
    ├─ 本盘编号.txt        程序识别用（请勿改动）
    ├─ 本盘说明.txt
    ├─ 本盘索引记录.csv    本盘上有哪些项目
    ├─ 校验清单\          每个项目逐文件的 SHA256
    ├─ 日志\
    ├─ 异常文件\          (校验失败被隔离的文件，下次自动重传)
    └─ 已封盘.txt          (存在 = 本盘已封盘)
```

**恢复**：不需要任何工具，把 `项目\项目名\` 直接复制回去就行。

---

## GUI 与 CLI 的关系

GUI（`bftool-gui.exe`）与 CLI（`bftool.exe`）**共用同一套 core 引擎和配置**（`bftool.toml` / `%APPDATA%\bftool\config.toml`），行为完全一致 —— GUI 绝不通过 shell 调 `bftool.exe`，而是直接调用引擎。`Reporter` trait 让 core 的输出在 CLI（终端）与 GUI（channel/进度条）各自渲染，而 core 永不 `println!`。

界面中文依赖系统已装中文字体（Windows 默认有微软雅黑）。

---

## 从 PowerShell 旧版迁移

如果你之前用的是 PowerShell 版（`Archive-Projects.ps1`），目录布局**完全兼容**：

- 盘内 `\本盘信息\本盘编号.txt`、`\本盘信息\校验清单\*.sha256.csv`、`\项目\` 不变
- 全局索引 `备份索引名单.csv` 列名不变（中文表头）
- 本盘索引 `本盘索引记录.csv` 列名兼容（多了一列 `Status`）

直接换 `bftool.exe` 继续用即可，已有的备份盘 `bftool verify` 也能验。

---

## 借鉴的优质项目

- [sourcefrog/conserve](https://github.com/sourcefrog/conserve) —— immutability、损坏隔离、`validate` 命令
- [restic/restic](https://github.com/restic/restic) —— 三大设计原则（easy/verifiable/secure）、`check` 命令哲学
- [rustic-rs/rustic](https://github.com/rustic-rs/rustic) —— append-only 默认、TOML 配置、CLI/core 分层
- [d3zd3z/rsure](https://github.com/d3zd3z/rsure) —— "未测过的备份不是备份"

---

## 从源码构建

需要 Rust stable + MSVC Build Tools（Windows 自带 link.exe）：

```cmd
git clone https://github.com/s-silt/bftool.git
cd bftool
cargo build --release                 :: 命令行 bftool.exe
cargo build --release -p bftool-gui   :: 桌面版 bftool-gui.exe
```

产物在 `target\release\bftool.exe` 与 `target\release\bftool-gui.exe`（静态链接 CRT，可直接拷走运行）。
默认 `cargo build` 只构建 CLI（纯命令行用户不必编译 eframe）；GUI 用 `-p bftool-gui` 显式构建。

---

## 项目结构（Cargo workspace）

```
bftool/
├─ Cargo.toml                  workspace 配置（成员、依赖版本、release profile）
├─ crates/
│   ├─ bftool-core/            核心引擎库（与界面无关）
│   │   └─ src/
│   │       ├─ lib.rs
│   │       ├─ config.rs       TOML 配置加载 + 校验
│   │       ├─ reporter.rs     Reporter trait（与界面解耦的输出接口）
│   │       └─ engine/
│   │           ├─ archive.rs       主归档流程
│   │           ├─ archive_test.rs  压缩包内部结构测试（7-Zip/WinRAR/Bandizip）
│   │           ├─ cruft.rs         OS 杂文件名单 + cruft-aware walker
│   │           ├─ drive.rs         备份盘检测/初始化/序号管理
│   │           ├─ durable.rs       写文件后 fsync 落盘
│   │           ├─ manifest.rs      清单生成 + 三重比对
│   │           ├─ safety.rs        路径安全检查 + 稳定性检测
│   │           ├─ txn.rs           事务标记
│   │           ├─ verify.rs        复查
│   │           ├─ find.rs          全局索引查询
│   │           ├─ status.rs        状态总览
│   │           └─ paths.rs         盘内路径常量
│   ├─ bftool-cli/             命令行入口（bftool.exe）
│   │   └─ src/
│   │       ├─ main.rs              调用 bftool_core::*
│   │       ├─ cli.rs               clap 子命令定义
│   │       └─ terminal_reporter.rs 终端渲染 Reporter
│   └─ bftool-gui/             桌面版入口（bftool-gui.exe，eframe/egui）
│       └─ src/
│           ├─ main.rs              eframe 入口（窗口/字体）
│           ├─ app.rs               App 壳 + 左侧栏 + 全局状态栏
│           ├─ reporter.rs          GuiReporter（core 输出 → channel/进度）
│           ├─ task.rs              后台任务 + 项目边界取消
│           └─ views/               7 视图 + theme（浅色扁平主题/组件）
└─ .github/workflows/build.yml CI（windows-latest，fmt+build+test+clippy + audit + release zip）
```

`bftool-core` 是业务引擎（与界面无关）。命令行 `bftool-cli` 与桌面版 `bftool-gui`（eframe/egui）
都**直接调用**它，不通过 shell 互相调 exe；`Reporter` trait 让 core 的输出在 CLI（终端）与
GUI（channel/进度条）各自渲染，而 core 永不 `println!`。

---

## License

[MIT](LICENSE)
