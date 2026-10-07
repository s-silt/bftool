# Desktop UI migration

The migration adds an independent desktop shell and bridge while preserving `crates/bftool-core`, CLI, egui, the original root Cargo manifests/lock and vendor patches. The UI currently has backup and history/verification/recovery pages; it does not migrate all old pages.

## Trust boundary

Only Rust calls `pipeline::backup`; the old archive/move operation is not used. Preview retains the actual BackupPlan in Rust and returns an opaque plan ID. Input revisions invalidate stale plans. Job IDs, snapshot sequences and request tickets isolate stale work. Byte counts cross IPC as decimal strings and are rendered with BigInt. Preview entries are paged and displayed with virtualization.

The bridge preserves KeepBoth, source files, core path/handle safety, restoration metadata, locking and publication transactions. Recovery leases bind to the verified journal/manifest and are consumed only when a job begins. Shutdown requests cancellation, waits for idle and then exits. UI error text is short and actionable; expandable diagnostics retain structured original details and clipboard success is reported only after success.

Tauri capabilities are confined to the local main window and explicit app commands. Backend validation remains mandatory. No filesystem/shell plugin, daemon or network service is added. Production loads bundled frontend resources with CSP and a restricted navigation origin. CDP/profile test hooks compile only in debug builds.

## Validation

The original Windows CI suite remains intact, including its explicit 33 safety exclusions. The new desktop CI job checks both independent Rust packages, frontend types/build/tests, bridge contract/recovery tests, x64 Release compilation and notice-aware packaging. Audit covers each committed Cargo lock. GUI behavior is not asserted by hosted CI.

Pre-integration local evidence: 19 bridge tests, 14 frontend tests, four virtual-list fixture assertions, debug GUI exercises and ordinary x64 Release error presentation at wide/420px windows and 96 DPI on an ARM64 Windows host using x64 emulation. These are separate validations, not a claim that every full GUI flow or physical target has passed. Core sources and root manifests/lock matched the verified e7a307cbaf71ee00e897e2e627f160a4488b1bf1 baseline.

Physical x64, 125/150/200% DPI, real backup volumes and broad WebView2/graphics compatibility need further acceptance. Release startup observed Chrome_WidgetWin_0 unregister error 1412 while normal shutdown exited zero; root cause is not established. Old debug receipts are not Release acceptance evidence.

## Licensing and build artifacts

`LICENSE` remains the project's MIT license. `THIRD_PARTY_NOTICES.txt` preserves original notices, Microsoft WebView2 loader notices and MPL source availability references for pinned covered components. `licenses/third-party-components.json` records a conservative Rust/npm inventory and reviewed lock hashes, without machine paths. `scripts/collect-third-party-notices.py` validates that fixed review; it does not silently discover or approve newly introduced dependencies. `scripts/package-desktop-release.mjs` rejects debug markers/non-x64/non-GUI PE inputs and writes a new package directory with combined license and SHA256SUMS. It does not sign, tag, upload or publish a release.
