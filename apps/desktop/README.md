# bftool Tauri desktop migration candidate

This is the first complete backup workflow using Tauri 2 + Vue 3 + TypeScript + Vite. The existing Rust core, CLI and egui application remain available. Version 0.1.11-rc.2 is a migration candidate, not a new stable release.

## Windows development and build

Validated with Rust 1.95, Node 24.15, npm 11.12, MSVC/Windows SDK and WebView2 Runtime. Install dependencies locally from the committed locks; no global npm installation is needed. Run from repository root:

```powershell
./scripts/project-npm.ps1 --prefix apps/desktop ci --ignore-scripts
./scripts/project-npm.ps1 --prefix apps/desktop test
./scripts/project-npm.ps1 --prefix apps/desktop run build
cargo test --manifest-path crates/bftool-desktop-bridge/Cargo.toml --locked
cargo build --manifest-path apps/desktop/src-tauri/Cargo.toml --release --locked --target x86_64-pc-windows-msvc --features custom-protocol
node scripts/package-desktop-release.mjs apps/desktop/src-tauri/target/x86_64-pc-windows-msvc/release/bftool-desktop.exe dist/desktop-x64
```

If CARGO_TARGET_DIR is set, adjust the executable path accordingly. For development, run `./scripts/project-npm.ps1 --prefix apps/desktop run tauri -- dev`. Dependencies are pinned separately for frontend, native shell and bridge; the original root workspace lock is unchanged.

The unsigned executable requires Microsoft WebView2 Runtime. Packaging includes the project MIT license and original third-party notices. The notice validator fails if reviewed dependency locks change; review licenses before updating their inventory. Release builds exclude debug CDP/profile environment hooks.

## Workflow and limits

Add files or directories, set each directory's suffix filter (shallow by default), choose a target, preview read-only, then copy with real progress and safe cancellation. Existing records support verification and recovery of interrupted backup tasks. Rust owns executable plans and recovery handles; the frontend cannot submit an arbitrary executable file list. Source files remain in place and existing destination files use KeepBoth.

Automated tests use synthetic temporary directories. Local validation covered bridge/contract/recovery and frontend state/error tests, debug UI fixtures and x64 Release startup/error presentation at 420px and 96 DPI. High DPI, physical x64 hardware, real backup drives and broad graphics compatibility remain unverified. A WebView2/Chrome_WidgetWin_0 unregister error 1412 was observed on the ARM64 test host even when ordinary shutdown returned zero; its cause is unresolved. Tauri is not a guarantee against graphics issues.

See [architecture and validation scope](../../docs/UI_MIGRATION.md). Never use production files to run test harnesses.

## Optional local debug harnesses

`native-ui-check.mjs` and `native-close-check.mjs` require a separately launched debug custom-protocol executable with an isolated `BFTOOL_TEST_PROFILE` and `BFTOOL_TEST_CDP_PORT=9432`. They create synthetic fixtures under `.build` and are not Release acceptance. `native-close-smoke.ps1 [-Idle] [-ExePath <debug exe>]` launches and closes only its owned process. No Release CDP hook exists.
