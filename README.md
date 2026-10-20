# Flint — Windows search launcher

Flint (working name) is a keyboard-driven Windows launcher. It does indexed
filesystem search with aggressive, progressive fuzzy matching, and keeps all
processing local.

**State: phase 4 of 5.** Open source under MIT OR Apache-2.0.

* **Implemented and tested on Linux:**
  * the search engine, index persistence/recovery, multi-volume catalog,
    startup snapshots, config and journal-less live updates
  * background-indexing governance
  * the file-operation pipeline (plan, review, revalidate, verify), with
    fault-injection tests
  * isolated, bounded previews, the opt-in content index and deep search,
    with archive-bomb, malformed-file and rapid-selection tests
* **Prototypes, not yet validated on Windows hardware:**
  * the Windows adapters (raw `$MFT` reader, USN journal,
    `ReadDirectoryChangesW`)
  * shell integration (`IFileOperation`, context menu, clipboard)
  * the launcher UI

  They are compile-checked and run end to end under Wine.
* **Not started:** plugins, diagnostics view and packaging (phase 5).
  PDF and media previews are not built yet.

The full status per requirement is in
[docs/requirements.md](docs/requirements.md).

Designed for any Windows 10/11 PC with 8 GB RAM (ADR-013):

* Indexing runs at background priority, one volume scan at a time, and
  pauses while you type.
* The index costs about 70 bytes per file.

## Layout

```
crates/flint-fuzzy      search engine: folding, bit-parallel kernels, name table, query parser, staged engine, live session
crates/flint-index      journal model, per-volume state machine, SQLite store, traversal fallback, NTFS simulator
crates/flint-ntfs       USN V2/V3 parser (fuzzed) + Win32 MFT/USN JournalSource + ntfs_probe example
crates/flint-preview    isolated preview/extraction helper (flint-preview-host), backends, limits, scheduler
crates/flint-ops        safe file operations (plan/revalidate/verify, compare, duplicates) + Windows shell integration
crates/flint-commands   command registry, /s web search, safe calculator
crates/flint-config     versioned settings (TOML), migrations/recovery, data paths, hotkey parsing
crates/flint-ui-win32   direct-Win32 launcher (virtual ListView, highlights, filter bar, file actions, timing)
crates/flint-bench      reproducible benchmarks and synthetic corpus
bench/cpp               C++ port of the hot kernels (Rust vs C++ comparison)
bench/results           recorded benchmark runs with machine/toolchain details
docs/                   architecture, decisions (ADRs), requirements, benchmarks, roadmap, dependencies
```

## Build and test

```sh
cargo test --release                       # 175 tests (Linux or Windows)
cargo build --release -p flint-bench
./target/release/flint-bench engine --n 1000000

# Windows binaries from Linux (MinGW); distribution builds use MSVC on Windows
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p flint-ui-win32
cargo build --release --target x86_64-pc-windows-gnu -p flint-ntfs --example ntfs_probe
cargo build --release --target x86_64-pc-windows-gnu -p flint-ops --example ops_probe
cargo build --release --target x86_64-pc-windows-gnu -p flint-preview --bins --examples
```

## Trying it on Windows

* `ntfs_probe.exe` lists volumes. `ntfs_probe.exe C:` (elevated) enumerates
  the MFT and reads the journal, printing timings and the behaviours still
  unverified on real hardware.
* `ops_probe.exe` runs copy, rename, new folder, move, recycle, permanent
  delete, and a replaced-file check through the real file-operation
  pipeline, on a scratch folder in `%TEMP%`.
* `preview_probe.exe FILE...` prints what the preview helper returns for
  each file, with timings.
* `flint-preview-host.exe` must sit next to `flint-ui-win32.exe`. It is the
  helper that reads file contents for the viewer and the content index.
* `flint-ui-win32.exe` indexes all volumes by settings. NTFS volumes use
  the change journal if the process can open it (elevated); otherwise they
  use traversal plus change notifications. Options:
  * `--root <folder>` indexes just one folder.
  * `--synthetic 1000000` uses an in-memory benchmark corpus.
  * `--selftest "q1|q2|{F3}"` types the queries automatically (`{F3}`,
    `{MENU}`, `{RENAME}` and `{COPYPATH}` stand for shortcuts).

  Press **Ctrl+Space** (configurable). Esc hides and Alt+F4 quits.
  Latencies, volume status and memory use are printed to the console.

## Keys

| Key | Action |
|---|---|
| Up/Down, PgUp/PgDn | Move; with Shift, extend the selection (batch operations) |
| Enter | Open (after checking the file is still the indexed one) |
| Ctrl+Enter | Show in Explorer |
| Shift+Enter | Open with… |
| Alt+Enter | Properties |
| F3 | Search deeper: weaker matches, then all remaining indexed candidates |
| F4 | Deep search: walk unindexed locations (skipped volumes, unreadable folders), within limits |
| F9 | Viewer pane on/off (off by default; `viewer.enabled`) |
| F2 | Rename |
| Ctrl+Del | Delete to the Recycle Bin (batches are previewed first) |
| Ctrl+Shift+Del | Delete permanently (always previewed; defaults to No) |
| Ctrl+Shift+C | Copy full path(s) |
| Alt+C / Alt+X / Alt+V | Copy / cut / paste files (Explorer-compatible clipboard) |
| Ctrl+Shift+N | New folder |
| Apps key, Shift+F10, right-click | Context menu: Flint's actions plus the Windows shell menu (Shift for extended verbs) |
| Tab | Filter bar (type, size, date) |

Rows are labelled "fuzzy", "scattered", "weak: path", "weak: similar" or
"unranked", so a weak candidate is never presented as a good match.
* **Settings:** `%LOCALAPPDATA%\Flint\config.toml`, created on first run.
  For a portable copy, put an empty `portable.marker` next to the exe; all
  data then lives in `data\` beside it.

## Query syntax

* Free text: fuzzy, typo- and order-tolerant.
* `"quoted phrase"`: literal.
* Partial paths: `proj\src\main`.
* Filters:
  * `ext:pdf,docx`
  * `type:file|folder|image|video|audio|doc|archive|code|exe`
  * `size:>10mb` (`>`/`<` are strict, `>=`/`<=` inclusive)
  * `modified:2024-01-01..` or `dm:week` (dates are local calendar days)
  * `path:docs`
* `/s query` searches the web (explicit only). `=1+2` or `2*(3+4)` uses the
  calculator.
* `content:word` or `content:"two words"` searches file contents. This
  needs `privacy.content_indexing = true` (off by default) and covers
  Documents and Desktop unless `content.roots` says otherwise. When off,
  the status line says so; it is never silently ignored.

## Key documents

* [decisions.md](docs/decisions.md): ADRs and open decisions needing the
  owner.
* [benchmarks.md](docs/benchmarks.md): status against the targets and the
  reference-PC protocol.
* [roadmap.md](docs/roadmap.md): phases 2–5.
