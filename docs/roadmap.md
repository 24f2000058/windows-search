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

## Phase 2: launcher and indexing

1. Multi-volume orchestrator:
   * volume arrival/removal (`WM_DEVICECHANGE`)
   * per-volume threads
   * journal wait (`BytesToWaitFor`)
   * background metadata refresh
2. Raw `$MFT` reader for the initial scan (names of all links, sizes, times).
3. Non-NTFS live updates with `ReadDirectoryChangesW` plus reconciliation.
4. Startup snapshot (ADR-004 open item); "loading index" state in the UI.
5. Config file (versioned, migrations, malformed-file recovery), portable
   mode, settings UI, and real hotkey customization.
6. Icons via an async `SHGetFileInfo`/`IShellItemImageFactory` cache.
   Theming (light/dark/high contrast). UIA audit.

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
