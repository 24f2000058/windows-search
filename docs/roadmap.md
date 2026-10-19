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
- [ ] Owner decisions: ADR-005 scope, reference PC, name/license.

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

## Phase 3: search UX and file operations

1. "Show weaker matches" / "show remaining candidates" actions, wired to
   `Options::through` and `exhaustive`.
2. Filters: local-time dates, a size/date UI, and exposing weak-match tiers
   in the UI.
3. File operations via `IFileOperation`:
   * revalidate by FRN and path right before acting
   * Recycle Bin by default, explicit permanent delete
   * batch preview, collision handling
4. Native shell context menu (`IContextMenu`, keeping third-party
   extensions) alongside a custom menu. Open with, properties, compare,
   optional duplicate detection.
5. Fault-injection tests for destructive-operation races.

## Phase 4: viewer, content and deep search

1. Preview host process with limits, generation-tagged requests and a cache
   (ADR-009), with backends added incrementally.
2. Selective content index with exclusions and explicit extraction states.
3. Bounded, cancellable deep-search fallback for unindexed scopes.
4. Archive-bomb, malformed-file and rapid-selection tests.

## Phase 5: plugins, diagnostics, release

1. Plugin host (ADR-008), permission UI, crash recovery tests.
2. Diagnostics view (per-volume state, checkpoints, latency, stages,
   failures, resources) with redaction-by-default export.
3. Installer (MSIX/WiX) plus portable zip, clean uninstall.
4. Update checks with user approval and signature verification before
   install.
5. Signed, reproducible release builds, SBOM (`cargo cyclonedx`), license
   CI gate.
