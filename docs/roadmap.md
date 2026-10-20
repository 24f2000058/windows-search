# Roadmap

Phase 1 (this repository state): prototypes and benchmarks of the search
engine, index model/persistence, NTFS adapter, commands and a Win32 UI
spike. See requirements.md for exact status.

## Phase 1 exit criteria — remaining

These need Windows hardware:

- [ ] Run `ntfs_probe.exe` on the reference PC and resolve the ADR-006
  "unverified" items.
- [ ] Measure the spike against the UI targets, prototype at least one
  framework, and close ADR-002.
- [ ] Windows CI (MSVC build + tests); compile-only checks run here today.
- [ ] Owner decisions: ADR-005 scope, final name (reference PC and license
  decided: ADR-013).

## Phase 2: launcher and indexing — done (pending hardware validation)

Done (see requirements.md for evidence):

- [x] Multi-volume catalog: per-volume workers, backoff, background
  metadata refresh, removal/re-arrival, aggregated coverage.
- [x] Volume discovery by drive type and settings; `WM_DEVICECHANGE`.
- [x] Raw `$MFT` reader with `ENUM_USN_DATA` fallback.
- [x] Non-NTFS live updates: `ReadDirectoryChangesW` + incremental
  reconciliation.
- [x] Startup snapshots (~3× faster cold load); "loading index" state.
- [x] Versioned config with migrations/recovery; portable mode; hotkey,
  search, theme and web provider from config.
- [x] File-type icons; light/dark/high-contrast palettes (WCAG AA tested).

Remaining from phase 2:

- [ ] Settings window (config is file-only today).
- [ ] UIA/accessibility audit and comctl32 v6 manifest.
- [ ] Excluded paths for journal (NTFS) volumes.
- [ ] Journal wait via `BytesToWaitFor` instead of 1 s polling. This needs
  idle-CPU measurement on Windows first.
- [ ] Hardware validation of everything marked Prototype (`ntfs_probe`,
  Windows CI).

## Phase 3: search UX and file operations — done (pending hardware validation)

Done (see requirements.md for evidence):

- [x] "Search deeper" (F3): weaker matches, then all remaining indexed
  candidates; weak rows labelled; the status line offers it only when it
  applies.
- [x] Filters: local-time, DST-correct dates; strict `>`/`<`; filter bar
  (type, size, date) that edits the query; size, modified and match
  columns.
- [x] File operations (`flint-ops`, ADR-015):
  * plan, reviewed preview, final revalidation (FRN or
    type/size/mtime), `IFileOperation`, verify by observation
  * Recycle Bin by default; permanent delete only with explicit
    confirmation
  * collisions configurable; batch preview
- [x] Custom menu above the native shell menu (`IContextMenu2/3`
  forwarding), with native delete/rename intercepted. Open, open with,
  reveal, copy/cut/paste, copy path, rename, move/copy to, new folder,
  properties, compare, duplicates.
- [x] Fault-injection tests for destructive-operation races.
- [x] Owner decisions recorded: reference PC, memory budget, license
  (ADR-013).
- [x] Background indexing governance (ADR-014) and a compact name table
  (68 B/entry at 1M entries, down from ~102 MB committed).

Remaining from phase 3:

- [ ] Run `ops_probe.exe` and the launcher on Windows. Check the Recycle
  Bin, conflict dialogs and third-party menu extensions.
- [ ] Measure the ADR-013 memory budget and ADR-014 pacing on the
  reference PC.
- [ ] Drag and drop out of the result list.

## Phase 4: viewer, content and deep search — done (pending hardware validation)

Done (see requirements.md for evidence):

- [x] Preview helper process (`flint-preview-host`), ADR-016:
  * timeout kill, restart with backoff, Job Object memory limit
  * generation-tagged latest-wins scheduler, bounded identity-keyed cache
  * backends: text, HTML, Office/ODF/EPUB text, ZIP listing, image
    info/thumbnail, hex
- [x] Viewer pane: off by default, F9; no helper while it is off or the
  launcher is hidden.
- [x] Selective content index, ADR-017: opt-in, FTS5, explicit extraction
  states, sensitive-location exclusions, governed background sync,
  `content:` filter.
- [x] Deep search, ADR-018: F4, bounded and cancellable, never enters
  excluded locations.
- [x] Archive-bomb, malformed-file and rapid-selection tests.

Remaining from phase 4:

- [ ] PDF text and preview (PDFium in the helper), WIC image decoding,
  opt-in Office preview handlers, media, OCR (ADR-009 candidates).
- [ ] Validate on Windows: shell thumbnails, the Job Object memory limit,
  and `preview_probe.exe` on real files.
- [ ] Deep search inside `path:` scopes that lie outside the index.

## Phase 5: plugins, diagnostics, release — done (pending hardware validation and owner decisions)

Done (see requirements.md for evidence):

- [x] Plugin host, ADR-019 ([plugins.md](plugins.md)):
  * versioned JSON protocol and a strict manifest
  * approval bound to a folder hash and the permission set
  * low integrity plus Job Object confinement on Windows
  * timeouts, backoff and session strikes
  * clamped output; sample plugin; recovery tests
- [x] Diagnostics (`/diag`): per-volume state and checkpoints, latency,
  stages, failures, resources, plugin health; redacted by default; copy
  and save.
- [x] Updates, ADR-020: manual by default, ed25519-signed manifest, no
  rollback, verified download, approval before download and install.
- [x] Installer and portable, ADR-021: per-user MSI with clean uninstall,
  deterministic portable zip, embedded manifest, GUI subsystem.
- [x] Release tooling: reproducible executables, SBOM, notices, license
  gate, checksums, CI and release workflows.

Remaining:

- [ ] Owner decisions: Authenticode signing service, update-signing key,
  final name (decisions.md).
- [ ] Run the CI and release workflows on GitHub (written and
  syntax-checked, not yet run).
- [ ] Validate on Windows hardware:
  * low-integrity plugin launch and the Job Object CPU cap (the
    child-process limit is verified in native Windows CI)
  * WinHTTP against GitHub
  * MSI install/upgrade/uninstall
  * the `/diag` window
- [ ] Plugin API 2 candidates: format handlers and metadata providers
  (ADR-008), and an AppContainer to restrict network and reads.
- [ ] A settings window (settings are still edited in `config.toml`).
