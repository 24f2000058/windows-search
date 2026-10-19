# Architecture decisions

Status values: **Accepted** (validated by a prototype/measurement in this
repo), **Provisional** (chosen, pending measurement on the Windows reference
PC), **Open** (needs a decision; recommendation given).

---

## ADR-001 Implementation language: Rust — Accepted

**Context.** The requirements ask for Rust vs C++ to be benchmarked before
choosing. The hot paths are the match kernels and memory-bandwidth-bound
scans; the rest of the system parses untrusted input (USN buffers, files for
preview, archives, plugin messages).

**Evidence.** `bench/cpp/kernels.cpp` is a line-by-line port of the Rust
kernels. Over 150 queries × 200,000 names, with identical hit counts, the
results were (bench/results/2026-10-19-linux-vm.md):

* Rust 164 ns/pair
* g++ -O3 159 ns/pair
* clang++ -O3 159–160 ns/pair

That is within ~3%, which is noise on this VM. A first, unfaithful C++ port
looked 25% faster, so faithful ports are required for such comparisons.

**Decision.** Rust. Raw performance is equivalent. What decides it is memory
safety for the large amount of untrusted-input parsing, plus `cargo` for
reproducible builds and SBOM generation. C++ stays acceptable for isolated
helper processes if a backend needs it (e.g. a C++ preview library).

## ADR-002 UI stack: direct Win32 — Provisional

**Context.** Targets: hotkey→interactive ≤100 ms, input latency p95 ≤50 ms
during indexing, idle CPU ≤0.5%, idle private bytes ≤50 MB.

**Prototype.** `crates/flint-ui-win32` contains:

* a hotkey (Ctrl+Space with a reported fallback)
* per-monitor-v2 DPI
* an owner-data virtual ListView with custom-drawn highlights
* plain text for screen readers via `LVN_GETDISPINFO`
* the live session over `PostMessage`

The binary is 5 MB (with debug line tables). Startup to ready took 60 ms under
Wine. It runs end to end under Wine/Xvfb (`docs/img/spike-wine-calc.png`).
Wine latency numbers are **not** Windows measurements.

**Not yet measured.** No framework candidate has been prototyped; this
environment cannot measure Windows UI latency or idle resources. Candidates to
compare on the reference PC, with the same measurement harness:

* Slint (Rust, native widgets/software renderer; check license terms for distribution)
* WinUI 3 / Windows App SDK (heavy runtime, best Fluent fidelity)
* egui (GPU, immediate mode; idle CPU must be verified)

**Decision rule.** Keep direct Win32 unless a framework meets all four
resource targets *and* materially reduces accessibility/theming work. Lean
risk with Win32: more code for theming (dark mode is undocumented for
classic controls), accessibility (UIA beyond standard controls) and
high-contrast.

## ADR-003 Search: in-memory columnar scan with exact prefilters — Accepted

**Design.** One `NameTable` with one slot per name link. Columns:

* names (shared arena, folded copy only when different)
* parent id
* a 64-bit character-presence mask
* size, mtime, flags, volume and source key

Paths are reconstructed from parent links, so a directory move is O(1).

Queries run as passes (Strong → Approx → Weak → Exhaustive). Each pass is a
parallel scan over slots not yet matched, with per-chunk bounded top-K heaps
and a ranked snapshot emitted after each pass.

**Why no inverted index.** A trigram posting index for 1M names is about
18M postings (~72 MB as `u32`), larger than the whole table. Masks plus
exact pigeonhole filters give sufficient selectivity. Measured on 1M entries
(4 threads): first ranked results arrive in ≤15 ms p95 for 8 of 10 query
classes, and full best-effort completion takes p95 12–72 ms.

**Correctness devices (each property-tested against brute force).**

* *Bit-parallel Damerau (OSA).* Hyyrö's bit-vector OSA runs in
  approximate-substring mode. It matched an explicit DP on 20k random
  Unicode cases.
* *Pigeonhole needles.* The query is split into k+1 pieces, and each piece
  also gets boundary-transposition variants. Any match with ≤k OSA edits
  contains one of these needles, so only those candidates run the OSA kernel.
  This was checked on 40k random cases × k=1..3.
* *Per-directory greedy prefix.* Greedy leftmost matching is optimal for
  subsequence tests. So "query is a subsequence of path + name" holds exactly
  when the rest of the query, after what the directory path matched, is a
  subsequence of the name. That makes path matching O(name) per entry.
* *Completeness tests.* The substring, fuzzy and subsequence stages each
  return exactly the brute-force match set on random corpora.

**Profiling lessons recorded.**

* Constructing a `str::find` searcher per candidate dominated the profile;
  precompiled `memmem::Finder`s fixed it.
* The `eval` call overhead (~125 instructions) was paid even by entries the
  mask rejects; an inlined mask-only rejection removed it.
* Random access into slot-indexed arrays during the per-directory
  precompute cost 18 ms; dense directory data brought it to 4 ms.

**Ranking.** Tiers dominate scores: exact > prefix > word-start > substring
> fuzzy > scattered > path/similarity > remaining. Weak tiers are labelled
as such in the UI.

**Short-query rule.** Normal intensity gives queries under 4 characters no
edits. One edit in three characters matched 122K noise names for `mgp` and
would have outranked the genuine scattered match.

## ADR-004 Persistence: SQLite (WAL) — Accepted, with an open startup item

* Schema: `volumes` (checkpoint state) and `entries`
  `(volume_id, frn, parent, name)` WITHOUT ROWID, with exact name collation
  because NTFS directories can be case-sensitive.
* Version in `user_version`; forward migrations; a newer schema is refused
  without touching the file; a corrupt file is moved aside, never deleted.
* Every journal batch and its checkpoint commit in one transaction. Journal
  application is idempotent, so `synchronous=NORMAL` is safe: a power loss
  only rolls back to an older, consistent checkpoint. Crash/restart
  randomized tests cover this.

Measured (~991k links): write 4.0 s, 55 MB on disk, cold load 2.1 s, 10k
changes in 0.58 s.

**Startup (phase 2).** Profiling showed the 2 s cold load is mostly index
rebuild (1.7 s), not SQLite (0.7 s). So snapshots store the name-table
*columns* per volume, not records.

* A snapshot is used only if its `commit_seq` equals the store's (schema v2
  adds a per-volume commit counter). It is FNV-checksummed, and every name
  span is bounds- and UTF-8-checked on import.
* Stale, corrupt or foreign snapshots fall back to SQLite.
* Snapshots are never written after a failed sync (memory may be ahead of
  the store).

Measured at ~1M entries: 0.62–0.74 s vs 1.93–2.41 s, including 0.19 s of
SQLite `quick_check`. Loading happens in the volume workers; the UI shows
"no matches yet — index still loading" instead of "no matches".

**Remaining options:** defer `quick_check` to the background; mmap the
snapshot.

## ADR-005 Elevation model — Open (recommendation)

`FSCTL_ENUM_USN_DATA`/`FSCTL_READ_USN_JOURNAL` need a volume handle, and that
requires administrator rights. The app runs as a standard user, so the
options are:

1. **Default: no elevation.** Traverse journal-less (non-elevated) volumes,
   then watch with `ReadDirectoryChangesW` (phase 2) plus periodic
   reconciliation. Coverage is reported honestly; it is never called live.
2. **Opt-in per-machine indexing service**, installed with explicit consent by
   the installer. It owns the MFT/USN work and serves the per-user process
   over an ACL'd named pipe. **Privacy issue:** a service sees names of files
   the user cannot open. Results must be filtered by the requesting user's
   access (at least directory traversal rights), or the service must be
   limited to single-user machines with that limitation stated. This must be
   designed and reviewed before implementation.
3. On-demand elevated helper (UAC prompt per session). Rejected as the
   default (a prompt every login); kept for one-off rescans.

**Recommendation:** 1 by default, 2 as an advanced opt-in. **Your decision
needed** on whether 2 is in scope for v1.

## ADR-006 NTFS metadata, hard links and ReFS — Provisional

* **Phase 2:** the initial scan reads the raw `$MFT` sequentially in 4 MB
  chunks. The parser applies update-sequence fixups and emits every
  non-DOS `$FILE_NAME` (all hard links), the `$STANDARD_INFORMATION` mtime
  and the unnamed `$DATA` size, and it merges extension records by base
  reference.
* If the MFT cannot be read, or any record is rejected (torn by a
  concurrent write, or corrupt), `FSCTL_ENUM_USN_DATA` also runs; applying
  both is idempotent. Changes during the read are replayed from the journal
  as before.
* `ENUM_USN_DATA`-only entries keep `META_STALE` and are refreshed by the
  catalog's bounded background loop.
* **Unverified on hardware:**
  * whether enumeration yields one record per hard link
  * the exact reason flags for removing a non-last link
  * whether `ERROR_INVALID_PARAMETER` signals a journal-ID mismatch

  The index is robust to either answer, because link sets are re-read via
  `FindFirstFileNameW`. `ntfs_probe.exe` reports these behaviours on the
  reference PC.
* ReFS uses 128-bit file IDs. The V3 parser rejects them explicitly
  (`WideFileId`), and ReFS volumes use traversal until a dedicated adapter
  exists.

## ADR-007 Concurrency — Provisional

The table sits behind an `RwLock`. Indexer writes happen in batches
(≤10k–50k records), and searches hold the read lock. Risk: a long search
delays a journal batch; a long batch delays a keystroke. Measure "UI input
latency during indexing" on the reference PC. If p95 >50 ms, move to epoch
snapshots (copy-on-write chunks) so readers never wait.

## ADR-008 Plugins — Designed, not implemented

* Out-of-process helpers, one per plugin, speaking a versioned
  length-prefixed JSON protocol over anonymous pipes. Capabilities:
  commands, result actions, format handlers, metadata providers.
* Each helper runs in a Job Object (memory/CPU/process limits, kill on
  close) with per-call timeouts, and is restarted with backoff and disabled
  after repeated crashes.
* Permissions are declared in the manifest (read file content, network,
  launch processes) and approved at install. The host only passes data the
  permission allows.
* Where feasible, the helper runs in an AppContainer/low-integrity token.
  **This is not a sandbox guarantee** and the UI will not claim one.
* The core works with every plugin disabled.

## ADR-009 Preview backends — Candidates only (phase 4)

| Format | Candidate | License | Isolation |
|---|---|---|---|
| text/source | built-in, size-capped, encoding detection | — | in-process |
| images | WIC (OS) | OS | helper process (codec vulns) |
| SVG | resvg | MPL-2.0 | helper |
| PDF | PDFium | BSD-3 | helper |
| Office | Windows preview handlers (`IPreviewHandler`) | OS/vendor | prevhost-style host process only; opt-in (runs third-party code) |
| EPUB | zip + XHTML text extraction | — | helper |
| archives | `zip` (MIT), `sevenz-rust` (Apache-2.0) | — | helper; listing only, no extraction to disk |
| SQLite | rusqlite, `immutable=1`, read-only | MIT | helper |
| PE metadata | `goblin` | MIT | helper |
| audio/video | Media Foundation | OS | helper, off by default |
| OCR | Windows.Media.Ocr | OS | helper, off by default |

Limits for all of them: time, memory (Job Object), input size, archive
depth/ratio and cache size. Requests carry the selection generation, and
stale results are dropped. With the viewer off, no backend is loaded or
scheduled.

## ADR-010 Configuration: versioned TOML — Accepted

The config is hand-editable TOML in the data directory, stamped with
`version`.

* Loading never fails. A missing file is created. A malformed file is moved
  to `config.toml.broken` and replaced with defaults. An invalid section or
  value is reset individually, so one typo does not discard all settings.
* A file from a newer version is used leniently and never overwritten.
* Migrations are ordered functions over the TOML tree, with a `.vN.bak`
  backup. Saves are atomic (temp file + rename, previous copy as `.bak`).
* Every event is a `Note` surfaced in the UI status line.
* Data directory: `%LOCALAPPDATA%\Flint\` (machine-specific index, not
  roamed), or `<exe dir>\data\` when `portable.marker` is next to the exe.
  Uninstall removes that one directory.

## ADR-011 Result icons by file type only — Accepted

Icons come from `SHGetFileInfoW(..., SHGFI_USEFILEATTRIBUTES)`, cached per
extension. Resolving per-file icons would load third-party icon-handler
shell extensions against the user's actual files. That is slower, and it
conflicts with "never execute files to preview them". Cost: an `.exe` shows
the generic application icon.

## ADR-012 Journal-less volumes: watch and reconcile — Accepted

* A watched traversal starts `ReadDirectoryChangesW` (recursive) *before*
  the initial walk.
* Changed directories are reconciled incrementally: re-list, diff against
  the index, walk only directories that are new, and commit atomically.
  Filesystem I/O happens outside the table lock.
* A buffer overflow forces a full re-walk. Watch failure falls back to
  periodic re-walks with backoff.
* Without a journal there is nothing to replay, so these volumes are
  re-walked at every start (snapshot makes them searchable meanwhile).
* Volume mode (journal vs traversal) is fixed at discovery time, and the
  two modes use distinct store keys, because their root identities differ.
* Excluded paths are honoured by walks and reconciliation. **Gap:**
  journal volumes still index excluded paths (open item).

## Open decisions needing the owner

1. **Elevation/service scope** (ADR-005).
2. **Reference PC** specification for the performance targets.
3. **Project name and license.** The workspace currently declares
   `MIT OR Apache-2.0` as a placeholder; "Flint" is a working name.
4. **Idle memory target vs index size.** The table uses ~75 bytes per entry,
   so 1M entries ≈ 74 MB RSS against the 50 MB provisional target. Options:
   * accept a per-entry budget (≈50 MB at ~650k entries)
   * compact columns (folded-offset elision, 32-bit times), est. −10 B/entry
   * drop derived caches when hidden (implemented: `trim_caches`)
5. **Code signing** certificate and update channel (needed for signed
   releases and update integrity).
