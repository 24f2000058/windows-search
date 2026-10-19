# Flint — Windows search launcher (phase 1: prototypes and benchmarks)

Flint (working name) is a keyboard-driven Windows launcher. It does indexed
filesystem search with aggressive, progressive fuzzy matching, and keeps all
processing local.

**State: phase 1 of 5.** The search engine, index model, persistence and
recovery are implemented and tested. The NTFS adapter and Win32 UI are
working prototypes that are compile-checked and smoke-tested under Wine, but
**not yet validated on Windows hardware**. Viewer, file operations, content
search, plugins and packaging are designed or not started. The full status
per requirement is in [docs/requirements.md](docs/requirements.md).

## Layout

```
crates/flint-fuzzy      search engine: folding, bit-parallel kernels, name table, query parser, staged engine, live session
crates/flint-index      journal model, per-volume state machine, SQLite store, traversal fallback, NTFS simulator
crates/flint-ntfs       USN V2/V3 parser (fuzzed) + Win32 MFT/USN JournalSource + ntfs_probe example
crates/flint-commands   command registry, /s web search, safe calculator
crates/flint-ui-win32   direct-Win32 UI spike (virtual ListView, highlights, timing)
crates/flint-bench      reproducible benchmarks and synthetic corpus
bench/cpp               C++ port of the hot kernels (Rust vs C++ comparison)
bench/results           recorded benchmark runs with machine/toolchain details
docs/                   architecture, decisions (ADRs), requirements, benchmarks, roadmap, dependencies
```

## Build and test

```sh
cargo test --release                       # 82 tests (Linux or Windows)
cargo build --release -p flint-bench
./target/release/flint-bench engine --n 1000000

# Windows binaries from Linux (MinGW); distribution builds use MSVC on Windows
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p flint-ui-win32
cargo build --release --target x86_64-pc-windows-gnu -p flint-ntfs --example ntfs_probe
```

## Trying it on Windows

* `ntfs_probe.exe` lists volumes. `ntfs_probe.exe C:` (elevated) enumerates
  the MFT and reads the journal, printing timings and the behaviours still
  unverified on real hardware.
* `flint-ui-win32.exe [folder]` indexes the folder (default: your profile)
  by traversal. `flint-ui-win32.exe --synthetic 1000000` uses a synthetic
  index instead. Press **Ctrl+Space**: arrows move, Enter reveals in
  Explorer, Esc hides, Alt+F4 quits. Latencies are printed to the console.
  `--selftest "q1|q2"` types the queries automatically.

## Query syntax

* Free text: fuzzy, typo- and order-tolerant.
* `"quoted phrase"`: literal.
* Partial paths: `proj\src\main`.
* Filters:
  * `ext:pdf,docx`
  * `type:file|folder|image|video|audio|doc|archive|code|exe`
  * `size:>10mb`
  * `modified:2024-01-01..` or `dm:week`
  * `path:docs`
* `/s query` searches the web (explicit only). `=1+2` or `2*(3+4)` uses the
  calculator.
* `content:` reports "not available yet". It is never silently ignored.

## Key documents

* [decisions.md](docs/decisions.md): ADRs and open decisions needing the
  owner.
* [benchmarks.md](docs/benchmarks.md): status against the targets and the
  reference-PC protocol.
* [roadmap.md](docs/roadmap.md): phases 2–5.
