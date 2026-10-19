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

**Open:** a 2 s cold load means search is partial during startup. The UI
must say "loading index" (never "no matches"). Options:

* an mmap-able snapshot of the name table, written at idle and validated
  against the SQLite checkpoint (recommended)
* lazy per-volume loading

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

* `FSCTL_ENUM_USN_DATA` supplies names, parents and attributes, but not
  size/mtime. Entries are marked `META_STALE` and refreshed by a bounded
  background loop (`refresh_stale_meta`). Size/date filters must say that
  results are partial until it finishes. A raw `$MFT` reader (sequential,
  gives all `$FILE_NAME`s, sizes and times) is planned to replace this for
  the initial scan.
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
