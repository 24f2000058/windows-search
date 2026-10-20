# Requirements traceability

Every requirement from the brief, with honest status. Nothing here is
complete until it has passed on Windows; "Tested" means automated tests in
this repo pass on the development host, which is Linux. Windows-only code is
compile-checked (`x86_64-pc-windows-gnu`) and, where noted, smoke-run under
Wine.

Legend:

* **Tested**: implemented with automated tests.
* **Prototype**: implemented, partially tested, or not hardware-validated.
* **Designed**: documented design, no code.
* **Not started**.

## 1. Platform and interface

| Requirement | Status | Evidence / gap |
|---|---|---|
| Windows target | Prototype | Cross-built PE binaries; no Windows CI yet |
| Installer + portable builds | Partial | Portable mode implemented and tested (`portable.marker` next to the exe → `<exe>\data\`, verified under Wine); installer and release packaging: phase 5 |
| Ctrl+Space default, customizable | Prototype | Default Ctrl+Space; `general.hotkey` in config.toml, parsed/validated (reserved combos rejected, tested); a taken hotkey is reported in the UI. No settings window yet |
| Compact live UI, icons, highlights, name + path, keyboard nav | Prototype | Live results with highlights, folder, size, local modified time and a match label; file-type icons (by type only — ADR-011); arrows/PgUp/PgDn, Shift+arrows multi-select, Tab to the filter bar, file-action shortcuts (README). Verified under Wine (`docs/img/phase3-*.png`) |
| Themes, custom keybindings | Partial | Light/dark following the system setting or forced in config, high contrast uses system colors; both palettes pass WCAG AA (tested). In-app keybinding customization not started |
| High-DPI | Prototype | Per-monitor v2, `WM_DPICHANGED` relayout; not tested on real displays |
| Accessibility | Prototype | Standard controls plus plain text via `LVN_GETDISPINFO`; no UIA audit or high-contrast work |
| Benchmark Rust vs C++ | Tested | ADR-001, `bench/cpp` |
| Benchmark Win32 vs framework | **Open** | Only Win32 prototyped; comparison needs the reference PC (ADR-002) |
| Standard user, elevate only when needed | Prototype | Runs unelevated; access-denied routes to traversal. Elevation path is ADR-005 (open) |

## 2. Filesystem indexing

| Requirement | Status | Evidence / gap |
|---|---|---|
| Persistent metadata index | Tested | `flint-index::store`, reload tests; per-volume startup snapshots validated by commit sequence (~3× faster cold start, corrupt/stale snapshots fall back to SQLite — tested) |
| All detected accessible volumes by default | Prototype | `discovery::plan` (tested) picks volumes by drive type and settings, every skip has a reason; catalog runs one worker per volume; re-planned on `WM_DEVICECHANGE`. Not run on hardware |
| MFT enumeration | Prototype | Raw `$MFT` reader: fixups, all link names, sizes, times, extension records (parser tested and fuzzed); `FSCTL_ENUM_USN_DATA` fallback when the MFT read fails or rejects records. Not run on hardware (`ntfs_probe` compares both) |
| USN incremental updates | Tested (sim) / Prototype (Win32) | State machine tested against the NTFS simulator; Win32 reader compile-checked |
| Stable IDs, parents, paths, metadata, journal IDs, checkpoints | Tested | FRN = segment + sequence; checkpoint in `volumes` |
| Create, modify, delete, rename, move | Tested | Randomized oracle test (6 seeds × 60 rounds) |
| Hard links | Tested (sim + parser) | One slot per link; raw MFT yields every `$FILE_NAME` (DOS aliases dropped); `Links` follow-ups re-read link sets; hardware behaviour unverified |
| Reparse points | Tested (traversal) | Indexed, never followed; NTFS attribute mapped |
| File-ID reuse | Tested | Sequence mismatch drops the stale entry and counts an inconsistency |
| Journal reset / wraparound / invalid checkpoint | Tested | `journal_reset`, `wraparound`, `records_purged_mid_read`, `checkpoint_ahead` tests |
| Interrupted indexing and recovery | Tested | Crash during enumeration is never reported complete; cancel test; crash-mid-commit restarts |
| Directory renames | Tested | `directory_move_updates_whole_subtree` |
| Other filesystems: adapters / traversal | Tested | `traverse.rs`; FAT/exFAT/ReFS → traversal |
| Removable / network / inaccessible / disconnected volumes | Tested (sim) / Prototype | Catalog: unavailable volumes wait with backoff and report it; removal hides entries from search, re-arrival reloads from snapshot (tested); network drives excluded by default, traversed (never journaled) when enabled |
| Never block UI | Prototype | Indexing in per-volume workers; startup loads in workers (UI shows "loading index"); scans apply records 4096 per lock hold and pause while the user types (ADR-014); compare/duplicate checks run off the UI thread. Not measured on hardware |
| Never claim complete falsely | Tested | `is_complete` requires Live or a clean traversal with zero skips; UI status shows incomplete coverage |
| SQLite or better | Tested | ADR-004 |
| Automated correctness/recovery tests | Tested | 47 index tests including fault injection |
| Background indexing with bounded resource use | Tested (governor) / Prototype (priorities) | ADR-014: background thread priority + EcoQoS, one heavy scan at a time (queued volumes reported), bounded pauses while typing, memory returned after scans; governor and catalog queueing tested; priority effects need hardware measurement |
| Live updates for non-NTFS volumes (`ReadDirectoryChangesW`) | Tested (reconcile) / Prototype (watcher) | Incremental directory reconciliation tested against the real filesystem; watcher feed with overflow → full rescan; live update verified under Wine. Volumes are re-walked at each start (no journal to replay) |

## 3. Search engine

| Requirement | Status | Evidence / gap |
|---|---|---|
| Exact, prefix, substring | Tested | Tiers 9–5; substring completeness property test |
| Edit distance: substitution, insertion, deletion, transposition | Tested | Bit-parallel OSA validated against DP; fuzzy completeness test |
| Subsequence / scattered | Tested | Completeness test |
| Token similarity, partial paths | Tested | Multi-token and path-segment tests, fuzzy dir segments |
| Unicode | Tested | 1:1 folding (case, diacritics, width); CJK tests |
| Nonsense queries degrade progressively | Tested | Weak LCS and path stages, exhaustive stage |
| Fallback order 1–6 | Tested | Passes Strong/Approx/Weak/Exhaustive; tiers rank strong > weak |
| Highlight actual matched chars incl. non-contiguous | Tested | `highlight()`; validity tests; visible in spike screenshots |
| Indexed retrieval, bounded ranking, caching, debouncing, cancellation, progressive rendering | Tested | Masks/pigeonhole/per-dir precompute, top-K heaps, session cache, debounce, cancel token, per-pass snapshots |
| Never synchronous filesystem scan per keystroke | Tested | Search only reads the in-memory table |
| Distinguish no match / incomplete index / inaccessible / canceled / exhausted | Tested | `Status` and `status_line` tests |
| Request progressively weaker matches / remaining indexed candidates | Tested | F3 escalates to weak matches, then every remaining indexed candidate (`ux::deeper_action`, tested); the status line offers it only when available; weak rows are labelled ("fuzzy", "scattered", "weak: path", "weak: similar", "unranked") so they are never presented as relevant |
| Controlled deep-search fallback | Not started | Phase 4 (bounded, cancellable traversal of unindexed scopes) |
| Filters: name, path, extension, type, size, date | Tested | Query parser and filter tests. Dates are local calendar days, DST-correct (`Clock`, tested with a 23-hour day; Windows zone rules via `SystemTimeToTzSpecificLocalTime`). `>`/`<` are strict. Filter bar (type/size/date dropdowns) edits the query text, so typed and picked filters cannot disagree (tested) |
| Content filter | **Not started** | Reported to the user as unavailable, never silently ignored |
| Performance targets | Partially measured | See benchmarks.md; Windows numbers pending |

## 4. Commands

| Requirement | Status | Evidence / gap |
|---|---|---|
| Extensible registry | Tested | `Registry`, duplicate ids rejected |
| `/s` explicit only, quotes preserved, safe encoding, default browser | Tested / Prototype | Encoding and template validation tested; `ShellExecuteW` after re-validation in the spike |
| Calculator without evaluating code | Tested | Bounded recursive descent, fuzzed |
| Plugin commands | Designed | ADR-008 |

## 5. Integrated viewer

| Requirement | Status |
|---|---|
| All preview formats, playback, OCR, isolation, limits, stale-preview prevention, cache invalidation, disabled = no work | **Designed** (ADR-009); no code |

## 6. Windows integration and file operations

| Requirement | Status | Evidence / gap |
|---|---|---|
| Custom + native shell context menu (`IContextMenu`) | Prototype | Our items above the native menu, `IContextMenu2/3` forwarding for extension-drawn items; native delete/rename intercepted into the safe pipeline (ADR-015). Shown under Wine (`docs/img/phase3-context-menu.png`); third-party extensions untested on Windows |
| Open / open with / reveal / copy / cut / paste / copy path / rename / move / delete / new folder / properties / batch / compare / duplicates | Prototype | All implemented (`actions.rs`, `flint-ops`): `ShellExecuteExW`, `SHOpenWithDialog`, `SHOpenFolderAndSelectItems`, CF_HDROP + Preferred DropEffect clipboard, `IFileOperation`, folder picker, `SHObjectProperties`; bounded byte compare and same-size → content-confirmed duplicates (tested). Pipeline verified under Wine via `ops_probe` (`SHFileOperationW` path, as Wine lacks `IFileOperation`) |
| Revalidate before destructive ops; Recycle Bin default; explicit permanent delete; race/link/collision handling | Tested (core) / Prototype (Win32) | ADR-015: plan → reviewed preview → final revalidation (FRN on NTFS, type/size/mtime on traversal volumes) → execute → verify by observation. 16 tests incl. fault injection: replaced/deleted/modified/unreadable between preview and execution, races during the batch check, partial/cancelled execution, roots, nested selections, move into itself, collisions, invalid names, links and hard links |
| Non-blocking shell operations | Prototype | `IFileOperation` shows its own progress UI and runs modally from the launcher; long compares/duplicate checks run on background threads |

## 7. Content search

| Requirement | Status |
|---|---|
| Selective persistent content index, exclusions, deep-search fallback, bounded extraction, metadata unaffected by failures | **Not started** (phase 4) |

## 8. Plugins

| Requirement | Status |
|---|---|
| Versioned API, isolated helpers, permissions, approval, timeouts, recovery, limits, core survives failure | **Designed** (ADR-008) |

## 9. Privacy, security, resources

| Requirement | Status | Evidence / gap |
|---|---|---|
| Local only, no telemetry | Tested by construction | No network code outside the explicit `/s` browser launch |
| Search history off by default | Tested by construction | Session cache is in-memory only, cleared on hide |
| Never execute input/files automatically | Prototype | Calculator does not evaluate code; files open only on explicit Enter/double-click/Open after revalidation; previews will never execute (phase 4) |
| Bounded workloads | Partial | Search top-K, deadlines, calculator limits, parser bounds, one heavy scan at a time, compare/duplicate read budgets (`files.*_limit_mb`); preview/archive/plugin limits not built |
| Lazy init, bounded caches, no continuous rescans | Partial | Caches trimmed on hide; journal-driven updates; idle journal polls no longer write to disk (tested); traversal volumes watched instead of rescanned |
| Redacted local diagnostics | Not started | |
| Update notification, approval, integrity | Not started | |
| Dependency/license records | Tested | `docs/dependencies.md` (generated) |

## 10. Configuration and diagnostics

| Requirement | Status |
|---|---|
| Settings (hotkeys, themes, ranking, fuzzy intensity, exhaustive, scope, …) | Partial | `config.toml` covers hotkey, theme, search (intensity, best effort, exhaustive, limits, debounce, deadline), indexing scope (volumes, removable, network, exclusions, extra folders) and resource use (background priority, concurrent scans, pause while typing), file operations (collisions, recycle confirmation, compare/duplicate limits), web search, viewer, privacy, updates, plugins. **No settings window yet** |
| Versioned config with migration/recovery | Tested | Versioned TOML, ordered migrations with backup, malformed file moved aside, per-section/per-value reset, newer-version files never overwritten, atomic saves (`flint-config`, 9 tests); index DB schema v1→v2 migration tested |
| Per-volume status, USN checkpoints, recovery state, latency, fuzzy stage, failures, resources, plugin errors | Partial: data exists (`VolumeIndexer::stats/state`, `Status.stages`), not yet exposed in a diagnostics view |

## 11–12. Validation and engineering rules

| Requirement | Status | Evidence / gap |
|---|---|---|
| ≥100k / 1M file tests | Tested | Benchmarks at 100k/1M; index tests to 120k; persistence at ~1M |
| Unicode, long paths, duplicates, random queries, misspellings, scattered | Tested | Except long paths: std handles `\\?\` but this is untested on Windows |
| Inaccessible volumes, journal resets, crashes, stale records | Tested (sim) | |
| Malformed files, rapid preview changes | Partial | Malformed USN buffers fuzzed; previews not built |
| Destructive-operation races | Tested | `flint-ops` fault-injection tests (see section 6) |
| Archive attacks, plugin failures | Not started | Depend on unbuilt features |
| Reproducible benchmarks with recorded conditions | Tested | `bench/results/` |
| Signed releases, reproducible builds, SBOM | Not started | |
