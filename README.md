# 归档备份工具 (bftool)

> SSD → 机械盘，**按文件夹**安全归档，SHA256 三重校验，绝不误删，可中断续传。
> 单文件 `bftool.exe`，**下载即用**，无任何外部依赖。

[![build](https://github.com/s-silt/bftool/actions/workflows/build.yml/badge.svg)](https://github.com/s-silt/bftool/actions/workflows/build.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

---

## 这是给谁用的

你在 SSD 上做项目（视频、图片、文档、代码……），做完一个，想把整个文件夹**完整复制**到一块机械硬盘冷存；一块盘写满了换下一块（自动编号「备份1、备份2、备份3…」）；过半年到一年定期复查，发现坏道立刻知道；**绝不希望源被误删**，也不想做去重/加密那么重的方案。

如果这描述符合你，这个工具就是给你的。

如果你需要的是：增量快照、加密、云同步、去重 → 请看 [restic](https://github.com/restic/restic)、[rustic](https://github.com/rustic-rs/rustic)、[conserve](https://github.com/sourcefrog/conserve)，那是别的问题域。

## 设计原则

1. **绝不删源**。校验通过后，源被**移动**到「已备份」目录，永远等着你自己决定删不删。
2. **不镜像**。从不使用 `robocopy /MIR` 这种"源删了就把备份也删了"的危险操作。
3. **每个项目独立提交**。一个项目损坏不影响其它的恢复。
4. **三重校验**。文件数 + 总字节 + 每文件 SHA256，任意一项不符 = 失败重做。
5. **每盘自洽**。盘里自带索引、校验清单、日志、README，离线也能自我说明。
6. **事务式**。"写标记 → 移动源 → 写索引 → 删标记"，宁可漏写索引（可人工补），也不要"写了索引却没真正归档"。
7. **新手友好**。错误信息一定带"具体怎么修"；默认值偏保守；不传任何参数 `bftool` 会给你看当前状态 + 可用命令。
8. **OS 杂文件自动排除**。`Thumbs.db` / `desktop.ini` / `.DS_Store` / `$RECYCLE.BIN` / `System Volume Information` 等系统注入文件**不进 manifest、不复制到机械盘、verify 也不当"清单外多余"报**。这些不是项目内容，排除它们让新手不会因为 antivirus 锁 Thumbs.db 莫名其妙整项目失败。

> **备份不等于"复制完了"**。借自 [rsure](https://github.com/d3zd3z/rsure) 的提醒：*backups aren't useful unless you've tested them.* 跑 `bftool verify <盘符>` 才算"已验证的备份"，建议每 6–12 个月一次。

## 快速上手

### 1. 下载

到 [Releases](https://github.com/s-silt/bftool/releases) 下载 `bftool-x86_64-pc-windows-msvc.zip`，解压得到 `bftool.exe` 和 `bftool.toml.example`。把它们放到任意目录（比如 `C:\Tools\bftool\`）即可。

> **不需要安装 Visual C++ Redistributable**：本工具静态链接 CRT，单 `.exe` 文件无外部 .dll 依赖。

### 2. 配置（30 秒）

把 `bftool.toml.example` 重命名为 `bftool.toml`，用记事本打开，把三个根目录改成你自己的：

```toml
ready_root    = "D:\\资料库\\待备份"
archived_root = "D:\\资料库\\已备份"
system_root   = "D:\\资料库\\备份系统"
```

### 3. 看一眼状态

在 `bftool.exe` 所在目录打开 PowerShell 或 cmd，跑：

```cmd
bftool
```

会显示当前的源目录、检测到的备份盘、待归档项目数、可用子命令。

### 4. 初始化一块空盘

插上一块**空的**机械硬盘（NTFS 格式），假设盘符是 `E:`：

```cmd
bftool init E
```

工具会：
- 自动取下一个编号（`备份1`、`备份2` …）
- 在盘里建好 `\项目\`、`\本盘信息\`（含日志、校验清单子目录）
- 写本盘说明文件

### 5. 演练 → 正式备份

把做完的项目文件夹挪到 `待备份\` 下，然后：

```cmd
bftool archive --dry-run    # 演练：只列出会做什么，不真正动文件
bftool archive              # 正式开跑
```

每个项目会经过：稳定性检测 → 体积检查 → 完整复制 → 三重校验 → 源在复制中是否变过 → 事务式提交。

中途 `Ctrl+C` 可以随时中断，下次跑 `bftool archive` 会从断点继续。

### 6. 盘写满了

工具会自动**封盘**（写 `已封盘.txt`）并提示。取下这块盘贴好标签离线收好，插上下一块空盘，再 `bftool init <新盘符>` → `bftool archive`。

### 7. 复查（防坏道）

```cmd
bftool verify E
```

会重算每个项目的 SHA256 跟清单比对，报告损坏/缺失/多余文件。

### 8. 找以前的项目

```cmd
bftool find 关键词
```

会在全局索引里搜，告诉你它在哪块「备份N」、盘内路径。

## 子命令速查

| 命令 | 作用 |
|---|---|
| `bftool` | 显示当前状态 + 可用子命令 |
| `bftool archive` | 归档：处理 待备份 下所有就绪项目 |
| `bftool archive --dry-run` | 演练（不复制、不写索引、不移动源） |
| `bftool archive --unsafe-no-hash --i-understand-this-can-miss-bitrot` | **危险**：跳过 SHA256 内容校验，挡不住静默损坏（比特腐烂）。必须两个开关同时传才生效，单独 `--unsafe-no-hash` 会被拒绝。仅适合海量素材类、且接受静默损坏不可见的场景；**不可再生的资料请保持完整 SHA256**。 |
| `bftool archive --limit 1` | 本次只处理一个项目 |
| `bftool init <盘符>` | 初始化一块空盘为下一个「备份N」 |
| `bftool init <盘符> --force` | 跳过防呆（系统盘/资料库盘/非空盘）；风险自负 |
| `bftool verify [盘符]` | 复查指定盘 |
| `bftool find <关键词>` | 在全局索引查项目位置 |
| `bftool drives` | 列出已识别的备份盘 |
| `bftool config-show` | 显示当前生效的配置 |

跑 `bftool <命令> --help` 看完整选项。

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

## 安全要点

1. **单份不算真备份**。每个项目默认只在一块盘上有一份；那块盘坏了就没了。**真正可靠是 3-2-1 ——至少两份介质 + 一份异地/云**。重要资料请跑两套盘或加云盘。
2. **气隙防勒索**。盘封盘后**拔下、离线收好**，勒索病毒碰不到离线的盘。
3. **定期复查**。每 6–12 个月一次 `bftool verify`。
4. **总索引留份**。`备份系统\备份索引名单.csv` 建议另存一份到云盘。
5. **写满即拔**。当前在写的盘以外都拔下，避免误写。

## 从 PowerShell 旧版迁移

如果你之前用的是 PowerShell 版（`Archive-Projects.ps1`），目录布局**完全兼容**：
- 盘内 `\本盘信息\本盘编号.txt`、`\本盘信息\校验清单\*.sha256.csv`、`\项目\` 不变
- 全局索引 `备份索引名单.csv` 列名不变（中文表头）
- 本盘索引 `本盘索引记录.csv` 列名兼容（多了一列 `Status`）

直接换 `bftool.exe` 继续用即可，已有的备份盘 `bftool verify` 也能验。

## 借鉴的优质项目

- [sourcefrog/conserve](https://github.com/sourcefrog/conserve) —— immutability、损坏隔离、`validate` 命令
- [restic/restic](https://github.com/restic/restic) —— 三大设计原则（easy/verifiable/secure）、`check` 命令哲学
- [rustic-rs/rustic](https://github.com/rustic-rs/rustic) —— append-only 默认、TOML 配置、CLI/core 分层
- [d3zd3z/rsure](https://github.com/d3zd3z/rsure) —— "未测过的备份不是备份"

## 从源码构建

需要 Rust stable + MSVC Build Tools（Windows 自带 link.exe）：

```cmd
git clone https://github.com/s-silt/bftool.git
cd bftool
cargo build --release
```

产物在 `target\release\bftool.exe`（静态链接 CRT，可直接拷走运行）。

## 项目结构（Cargo workspace）

```
bftool/
├─ Cargo.toml                  workspace 配置（成员、依赖版本、release profile）
├─ crates/
│   ├─ bftool-core/            核心引擎库（与界面无关）
│   │   └─ src/
│   │       ├─ lib.rs
│   │       ├─ config.rs       TOML 配置加载
│   │       ├─ ui.rs           终端输出辅助（后续会改为事件流）
│   │       └─ engine/
│   │           ├─ archive.rs  主归档流程
│   │           ├─ drive.rs    备份盘检测/初始化/序号管理
│   │           ├─ manifest.rs 清单生成 + 三重比对
│   │           ├─ safety.rs   路径安全检查 + 稳定性检测
│   │           ├─ txn.rs      事务标记
│   │           ├─ verify.rs   复查
│   │           ├─ find.rs     全局索引查询
│   │           ├─ status.rs   状态总览
│   │           └─ paths.rs    盘内路径常量
│   └─ bftool-cli/             命令行入口（bftool.exe）
│       └─ src/
│           ├─ main.rs         调用 bftool_core::*
│           └─ cli.rs          clap 子命令定义
└─ .github/workflows/build.yml CI（windows-latest，cargo check + clippy + release zip）
```

`bftool-core` 是业务引擎，未来桌面版（Tauri / egui）会**直接调用**它，
不会通过 shell 调 `bftool.exe`。

## License

[MIT](LICENSE)
