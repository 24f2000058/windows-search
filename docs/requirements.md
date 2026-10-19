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
| Installer + portable builds | Not started | Phase 5 (MSIX or WiX; portable = zip with `portable.marker`) |
| Ctrl+Space default, customizable | Prototype | `RegisterHotKey` with reported fallback when taken; no settings UI |
| Compact live UI, icons, highlights, name + path, keyboard nav | Prototype | Spike has live results, highlights, paths, arrows/PgUp/PgDn/Enter/Esc. **No icons yet** |
| Themes, custom keybindings | Not started | |
| High-DPI | Prototype | Per-monitor v2, `WM_DPICHANGED` relayout; not tested on real displays |
| Accessibility | Prototype | Standard controls plus plain text via `LVN_GETDISPINFO`; no UIA audit or high-contrast work |
| Benchmark Rust vs C++ | Tested | ADR-001, `bench/cpp` |
| Benchmark Win32 vs framework | **Open** | Only Win32 prototyped; comparison needs the reference PC (ADR-002) |
| Standard user, elevate only when needed | Prototype | Runs unelevated; access-denied routes to traversal. Elevation path is ADR-005 (open) |

## 2. Filesystem indexing

| Requirement | Status | Evidence / gap |
|---|---|---|
| Persistent metadata index | Tested | `flint-index::store`, reload tests |
| All detected accessible volumes by default | Prototype | `win::list_volumes`; no multi-volume orchestrator yet |
| MFT enumeration | Prototype | `FSCTL_ENUM_USN_DATA`; raw `$MFT` reader planned (ADR-006); not run on hardware |
| USN incremental updates | Tested (sim) / Prototype (Win32) | State machine tested against the NTFS simulator; Win32 reader compile-checked |
| Stable IDs, parents, paths, metadata, journal IDs, checkpoints | Tested | FRN = segment + sequence; checkpoint in `volumes` |
| Create, modify, delete, rename, move | Tested | Randomized oracle test (6 seeds × 60 rounds) |
| Hard links | Tested (sim) | One slot per link; `Links` follow-ups re-read the true link set; hardware behaviour unverified |
| Reparse points | Tested (traversal) | Indexed, never followed; NTFS attribute mapped |
| File-ID reuse | Tested | Sequence mismatch drops the stale entry and counts an inconsistency |
| Journal reset / wraparound / invalid checkpoint | Tested | `journal_reset`, `wraparound`, `records_purged_mid_read`, `checkpoint_ahead` tests |
| Interrupted indexing and recovery | Tested | Crash during enumeration is never reported complete; cancel test; crash-mid-commit restarts |
| Directory renames | Tested | `directory_move_updates_whole_subtree` |
| Other filesystems: adapters / traversal | Tested | `traverse.rs`; FAT/exFAT/ReFS → traversal |
| Removable / network / inaccessible / disconnected volumes | Tested (sim) / Prototype | Unavailable keeps checkpoint, stays searchable, reports incomplete; no device-arrival handling yet |
| Never block UI | Prototype | Indexing on worker threads; RwLock contention risk (ADR-007) |
| Never claim complete falsely | Tested | `is_complete` requires Live or a clean traversal with zero skips; UI status shows incomplete coverage |
| SQLite or better | Tested | ADR-004 |
| Automated correctness/recovery tests | Tested | 27 index tests including fault injection |
| Live updates for non-NTFS volumes (`ReadDirectoryChangesW`) | Not started | Phase 2 |

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
| Controlled deep-search fallback | Not started | Phase 4 (bounded, cancellable traversal of unindexed scopes) |
| Filters: name, path, extension, type, size, date | Tested | Query parser and filter tests. Dates are UTC (local-time handling is a gap) |
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
| Custom + native shell context menu (`IContextMenu`) | Not started | |
| Open / open with / reveal / copy / cut / paste / copy path / rename / move / delete / new folder / properties / batch / compare / duplicates | Not started | Spike only reveals in Explorer after an existence check |
| Revalidate before destructive ops; Recycle Bin default; explicit permanent delete; race/link/collision handling | Designed | Plan: `IFileOperation` with `FOFX_RECYCLEONDELETE`; revalidate by FRN (`OpenFileById`) and path immediately before acting; preview/confirm batches |
| Non-blocking shell operations | Not started | |

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
| Never execute input/files automatically | Prototype | Calculator does not evaluate code; Enter reveals rather than executes in the spike |
| Bounded workloads | Partial | Search top-K, deadlines, calculator limits, parser bounds; preview/archive/plugin limits not built |
| Lazy init, bounded caches, no continuous rescans | Partial | Caches trimmed on hide; journal-driven updates |
| Redacted local diagnostics | Not started | |
| Update notification, approval, integrity | Not started | |
| Dependency/license records | Tested | `docs/dependencies.md` (generated) |

## 10. Configuration and diagnostics

| Requirement | Status |
|---|---|
| Settings (hotkeys, themes, ranking, fuzzy intensity, exhaustive, scope, …) | Engine options exist (`Options`, `Intensity`); **no config file/UI yet** |
| Versioned config with migration/recovery | Not started |
| Per-volume status, USN checkpoints, recovery state, latency, fuzzy stage, failures, resources, plugin errors | Partial: data exists (`VolumeIndexer::stats/state`, `Status.stages`), not yet exposed in a diagnostics view |

## 11–12. Validation and engineering rules

| Requirement | Status | Evidence / gap |
|---|---|---|
| ≥100k / 1M file tests | Tested | Benchmarks at 100k/1M; index tests to 120k; persistence at ~1M |
| Unicode, long paths, duplicates, random queries, misspellings, scattered | Tested | Except long paths: std handles `\\?\` but this is untested on Windows |
| Inaccessible volumes, journal resets, crashes, stale records | Tested (sim) | |
| Malformed files, rapid preview changes | Partial | Malformed USN buffers fuzzed; previews not built |
| Destructive-operation races, archive attacks, plugin failures | Not started | Depend on unbuilt features |
| Reproducible benchmarks with recorded conditions | Tested | `bench/results/` |
| Signed releases, reproducible builds, SBOM | Not started | |
