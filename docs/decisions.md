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

* names (shared arena, folded copy only when different, stored right after
  the display name so it needs no offset column)
* parent id
* a 64-bit character-presence mask
* size, mtime (u32 seconds, exact side table for out-of-range times),
  flags, volume and source key

That is 43 bytes of fixed columns per slot plus name bytes (ADR-013).

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

## ADR-008 Plugins — Implemented (commands and result actions; see ADR-019)

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

## ADR-009 Preview backends — Candidates (built-in subset implemented, see ADR-016)

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

## ADR-013 Reference PC, memory budget and license — Accepted (owner input)

The owner specified the reference PC and asked for low memory use and an
open-source project.

**Reference PC class.** Any standard Windows 10 (22H2) or Windows 11
machine with **8 GB RAM**. Performance is measured on a modest example of
that class, not a developer workstation:

* 4 cores (e.g. Core i5 8th gen or Ryzen 5 2000 series)
* 8 GB RAM, SATA SSD (one HDD run for disk-contention behaviour)
* 300k–1M files on the system volume
* Balanced power plan, Defender enabled

**Memory budget.** An in-memory index costs memory per file, so a flat
50 MB idle target cannot hold for 1M files. The budget is:

* **Idle** (window hidden, no scan running, viewer off): private bytes
  ≤ **25 MB + 70 bytes per indexed entry**. That is about 60 MB at 500k
  entries and 95 MB at 1M, which is 1.2% of 8 GB.
* **While scanning:** bounded by one scan at a time (ADR-014).

Measured so far (Linux, 1M synthetic entries; Windows numbers pending):

| Version | Name table |
|---|---|
| Before | ~102 MB committed (75 B/entry plus up to 50% `Vec` growth slack) |
| Now | 68 MB (68 B/entry: 43 B fixed columns plus name bytes) |

Changes that produced this:

* folded names stored without an offset column (−4 B/entry)
* times stored as u32 seconds, exact via a side table for out-of-range
  values (−4 B/entry)
* `shrink_to_fit` after every bulk load or scan, which returns the growth
  slack. This matters on Windows, where capacity is committed memory.

Derived search caches (chain masks: 8 B/entry; directory index) are
dropped whenever the window hides.

Further options, to apply if hardware measurements exceed the budget:

* Real Windows names are mostly mixed case, so the folded copy roughly
  doubles name bytes. An ASCII case bitmap instead could save an estimated
  10–15 B/entry on real data.
* A 40-bit size column (−3 B/entry).

**License.** Dual **MIT OR Apache-2.0**, the Rust ecosystem convention. It
is compatible with every dependency (all permissive, see
dependencies.md), and Apache-2.0 adds an explicit patent grant.
`LICENSE-MIT` and `LICENSE-APACHE` are in the repository root. "Flint"
stays a working name until the owner picks the final name (several
unrelated projects already use it).

## ADR-014 Background indexing must not compete with the user — Accepted

Indexing runs in the background and must not be noticeable on an 8 GB
machine. A shared `Governor` (`flint-index::governor`) applies these rules:

* **Background priority.** Worker threads enter
  `THREAD_MODE_BACKGROUND_BEGIN`: lowest CPU priority, very-low I/O
  priority, low memory priority. On Windows 11 they also opt into EcoQoS,
  which runs them on efficiency cores at lower clock. The search and UI
  threads stay at normal priority. Setting:
  `indexing.background_priority`.
* **One heavy scan at a time.** Full enumerations and traversals queue for
  a permit (setting `indexing.concurrent_scans`, default 1). Several disks
  are never thrashed at once, and peak memory is that of one scan. Queued
  volumes say "queued" in the status line, never "indexing".
* **Yield to typing.** Each keystroke marks the user active for 500 ms.
  Workers pause between 4096-record batches while the user is active. A
  pause lasts at most 250 ms, so indexing always progresses. Setting:
  `indexing.pause_while_typing`.
* **Short lock holds.** Records are applied to the shared table 4096 at a
  time, not in 50k batches, so a search waits milliseconds for the lock
  during a scan.
* **Memory returned.** After each bulk load or scan the table is shrunk if
  more than 1/8 of it is slack.

The governor only paces work and never drops any, so index correctness
does not depend on it. It is tested for:

* serialized scans, with queued status
* a queued scan that can still be stopped
* bounded pauses
* the setup hook running on every worker

It is not yet measured on hardware. The idle-CPU target also needs the
open `BytesToWaitFor` item: the journal is still polled every second, a
cheap read with no disk writes.

## ADR-015 File operations: plan, review, revalidate, verify — Accepted

The index can be stale, so no operation trusts it. `flint-ops` (tested on
any OS with a fault-injecting fake filesystem) runs every operation in four
steps.

1. **Plan** against the live filesystem. Items are refused, with a reason,
   when they are:
   * missing, replaced (file ID differs), changed between file and folder,
     or modified (traversal volumes)
   * a drive or share root
   * inside another selected folder (the outer folder covers them)
   * a duplicate in the selection
   * a move or copy into themselves, or a move into their current folder
   * renamed to an invalid name: separators, `..`, reserved device names,
     trailing dots or spaces, forbidden characters
   * a rename or new folder whose name is already taken

   The plan also notes links (the link is acted on, not its target), other
   hard-link names, and destination collisions.
2. **Review.** The user sees a preview (capped at 15 lines plus "and N
   more") for batches, permanent deletes, and anything with warnings or
   collisions. Permanent delete defaults to "No" and cannot run without
   explicit confirmation. Delete goes to the Recycle Bin by default.
3. **Revalidate** immediately before acting, against both the index
   identity and what the preview showed. Items that changed are skipped,
   never acted on.
4. **Execute and verify.** One `IFileOperation` per batch provides the
   progress UI, undo, elevation prompts and conflict dialogs. Collision
   handling is configurable: ask, keep both, or skip. Each item's outcome
   is then *observed*, not taken from the executor. Under Wine, which lacks
   `IFileOperation`, the same plan runs through `SHFileOperationW`.

Identity:

* **Journal (NTFS) volumes:** the stored FRN is compared with the live file
  ID. NTFS sequence numbers make a same-named replacement detectable.
* **Traversal volumes:** synthetic IDs, so the check is type + size +
  mtime. This is weaker, and documented as such.

A window of milliseconds remains between the final check and the shell
acting on the path; the shell's own checks apply within it.

**Context menu.** Our items come first, then the native shell menu
(`IContextMenu`, with `IContextMenu2/3` message forwarding, so third-party
extensions keep working).

* The native "delete" and "rename" verbs are intercepted and routed
  through the pipeline above.
* The native menu is offered only when every selected entry still matches
  the index.
* For items in different folders, only our menu is shown.

Opening a file happens only on an explicit request (Enter, double-click,
"Open"), after revalidation.

## ADR-016 Previews run in an isolated, bounded helper — Accepted

Every parser of file contents runs in `flint-preview-host`, a separate
process the launcher talks to over pipes, using a length-prefixed binary
protocol. The reader bounds-checks every length and refuses frames above
8 MB before allocating. Windows shell thumbnails also run there, because
thumbnail handlers are third-party code.

**Containment**
* **Crashes and hangs.** A request that does not answer within
  `viewer.timeout_ms` (default 3 s) kills the helper. A crash or timeout
  costs only that file; the helper restarts on demand with exponential
  backoff (100 ms doubling to 30 s).
* **Memory.** On Windows the helper runs in a Job Object: memory limit
  `viewer.helper_memory_mb` (256), kill on job close, no crash dialogs.
* **Not a security sandbox.** Process isolation limits the damage of a
  parser bug, and nothing claims more.

**Limits inside the helper** (`Limits`)
* bytes read per file
* characters returned
* archive entries listed
* bytes inflated per archive member
* declared compression ratio
* thumbnail edge

**Backends** (built in, no new native code; `miniz_oxide` for deflate)
* text, with BOM, UTF-8 and Windows-1252 detection
* HTML, with scripts and styles dropped
* DOCX, XLSX, PPTX, ODF and EPUB text
* ZIP listing
* image dimensions from headers, plus a shell thumbnail on Windows
* hex of the first 4 KB of other binaries

Programs are never previewed or run. PDF is stated as unsupported rather
than shown wrong.

**Archives** are listed, never extracted, and nested archives are never
opened. Zip-bomb heuristics:
* huge declared totals or compression ratios
* overlapping entries (the "quoted overlap" construction)
* nested archives

A member is inflated only under the ratio limit, and only up to
`max_inflate` whatever its header claims.

**Scheduling** (`scheduler::Viewer`)
* Each request carries the selection generation, and only the latest wins.
* Selection changes are debounced (60 ms).
* Results arriving after the selection moved on are dropped.
* The cache is keyed by path, size, mtime and file ID, and bounded by bytes
  and entries.

**Off means off.** The viewer is off by default (`viewer.enabled`, F9 per
session). While it is off, and whenever the launcher is hidden, no viewer
thread or helper exists and no file is read.

**Tests**
* wire round-trips and every truncation
* crash, hang, backoff and missing-helper cases against the real binary
* rapid selection: 200 selections cause at most 2 previews and exactly 1
  delivery
* stale results dropped
* cache bounds
* zip bombs and overlap bombs
* thousands of mutated archives and random inputs per backend, with no
  panic

Other candidates in ADR-009 (PDFium, WIC decoding in the helper, Office
preview handlers, media) remain open. Each would be added as a helper
backend under the same limits.

## ADR-017 Selective content index — Accepted

Content indexing reads file contents, so it is **opt-in**
(`privacy.content_indexing`, default off).

**What is read**
* Only files under `content.roots` (default: Documents and Desktop).
* Only the configured extensions (text, markup and Office formats).
* Only files under `content.max_file_mb`.
* Never anything under `indexing.excluded_paths`, `content.excluded_paths`,
  or built-in sensitive locations (AppData, `.ssh`, `.gnupg`, cloud CLI
  credential folders, Windows credential stores). This holds whatever the
  roots say.

**How it is stored and searched**
* Text comes from the isolated helper (ADR-016). The filename index never
  depends on extraction succeeding.
* Storage is a separate SQLite database, `content.db`, using FTS5 with
  `unicode61` and diacritics removed. Content data can be deleted on its
  own.
* Every file has an explicit state, with the reason kept: `indexed`,
  `truncated`, `unsupported` (e.g. PDF), `too_large` (never read) or
  `failed` (including helper crashes and timeouts).
* Files are re-extracted when size or mtime change and dropped when they
  vanish.
* The syncer runs at background priority, under the same one-heavy-scan
  rule and typing pauses as indexing (ADR-014). It re-syncs when the
  filename index changes (at most once a minute) or every
  `content.resync_minutes`.
* `content:` filters go through the engine (`ContentMatch`).
  * User text is turned into quoted FTS terms, so it is never interpreted
    as FTS syntax.
  * Several words: `content:"two words"`.
* Coverage caveats appear in the status line: "content index: N file(s)
  not yet read", or a note that the result cap was hit.
* With the index off, `content:` says so and is never silently ignored.

## ADR-018 Deep search of unindexed locations — Accepted

F4 walks, on demand only, locations the index does not cover:
* volumes skipped by settings (removable, network, optical, not listed)
* folders the last traversal could not read

Rules:
* **Bounded.** The walk is breadth-first and cancellable. It stops at
  `deep_search.max_entries`, `max_seconds` or `max_depth`, and the report
  names the limit hit. A partial walk is never shown as a full one.
* **No-go areas.** The walk never enters excluded or sensitive folders and
  never follows links.
* **Ranking and results.** Ranking uses the normal engine over a
  temporary table. Hits are inserted into the shared table under a
  reserved volume, labelled "deep (unindexed)", and act like other results
  (file operations use the weaker type, size and mtime check). They are
  removed when the query changes or the launcher hides.

## ADR-019 Plugins: approved, confined helper processes — Accepted

Implements ADR-008 for **commands** (prefix-triggered results) and
**result actions** (context-menu entries on selected files). Format
handlers and metadata providers are not in API 1. Protocol and author
guide: [plugins.md](plugins.md).

* **Off by default** (`plugins.enabled`). With plugins off, no plugin
  folder is read. With plugins on, folders are discovered and hashed at
  startup, but nothing runs until used.
* **Approval is bound to content.** The approval records a SHA-256 over
  every file in the plugin's folder (links refused) plus the permission
  set. Any change means approve again. The folder is re-hashed before every
  process start, so files swapped after approval are refused. Approvals are
  stored outside the plugin folders and fail closed if unreadable.
* **Permissions limit what Flint does**: passing paths, opening URLs,
  allowing child processes. `network` is declared but unenforceable. The
  approval prompt says this, and that isolation "is not a security
  sandbox".
* **Confinement (Windows):**
  * a low-integrity token (falling back to normal integrity in the same
    job, which `/diag` reports)
  * a Job Object: memory limit, 25% hard CPU cap, one active process
    unless `launch_processes`, UI restrictions, kill on close
  * only the pipe handles are inherited
  * no AppContainer, so network and read access are not restricted
* **Recovery:**
  * per-request timeout (manifest, at most 10 s), then kill
  * restart with exponential backoff
  * malformed, oversized (> 1 MiB) or mismatched replies kill the process
  * 3 strikes per session disable the plugin until restart
  * `error` replies are not strikes
* **Output is untrusted:**
  * at most 20 items
  * text clamped, control and bidi characters stripped
  * `open_url` stripped without the permission and re-checked at Enter
* **Resources:** queries are latest-wins on a per-plugin worker thread.
  Every plugin process is stopped when the launcher hides.
* **Tests:** 11 unit tests and 13 against a real plugin process, covering protocol
  round trip, withheld paths, crash/hang/garbage/oversize/wrong-id
  recovery, strikes, error answers, flooding, permission stripping, API
  mismatch, changed files and latest-wins. The confined spawn runs in CI
  on native Windows (GitHub's Windows Server 2022 runner), where the job's
  one-process limit is verified to refuse a child process. Wine enforces
  neither that limit nor integrity levels. The low-integrity token is
  applied but not yet checked by a test.

## ADR-020 Updates: manual by default, signed manifest, verified download — Accepted

* `updates.mode`: `off`, `manual` (default: the network is contacted only
  on `/update`) or `notify` (at most every `check_interval_hours`, first
  check 15 minutes after start, status line only). Config v1 files had
  `notify` written as an unused default; migration v1→v2 turns that into
  `manual`.
* **Signed manifest.** A release publishes `update.json` and
  `update.json.sig`. The signature is ed25519 over
  `"flint-update-manifest-v1\n" + bytes`, with a key compiled into the build
  (`crates/flint-update/update-keys.txt`; several keys allow rotation). The
  manifest is parsed only after the signature verifies. The product must be
  `flint`, URLs must be https, and assets carry size and SHA-256.
* **Policy:**
  * only a strictly newer version is offered (no rollback)
  * pre-releases are offered only to pre-release builds
  * the download is capped at the signed size and must match the digest
    before anything is offered to run
* **Approval at every step:**
  * "Download and verify?"
  * then "Verified. Install now?", which starts `msiexec` and closes Flint
  * portable copies get the verified zip revealed with instructions; no
    self-replacement
* **Transport:** WinHTTP (system TLS, certificates and proxy; https→http
  redirects refused). Integrity does not depend on the transport.
* **No key yet.** The project has no release key (open decision), so
  current builds report that they cannot verify updates and offer the
  releases page. They never download.
* **Tests:** signature acceptance and rejection, tampering at any byte,
  context separation, unsafe signed content, version policy, and download
  size/digest checks. Wine runs exercised the WinHTTP path against a local
  server: genuine, padded and tampered releases.

## ADR-021 Packaging and release — Accepted

* **Installer:** a per-user MSI (`installer/flint.wxs`, built with `wixl`
  on Linux).
  * no elevation; installs to `%LOCALAPPDATA%\Programs\Flint`
  * Start menu shortcut, plus a separate "Autostart" feature (HKCU Run)
  * major upgrades; downgrades refused
  * uninstall runs `flint --quit`, then `flint --cleanup-data`, which
    removes the index and caches but keeps settings, plugins and approvals
  * `REMOVEDATA=1` removes the whole data folder
  * cleanup refuses any folder that is not a Flint data folder
  * verified under Wine: install, launch, uninstall with and without
    `REMOVEDATA`
* **Portable:** a zip with `portable.marker`; all data stays in `data\`.
* **Executable:** release builds use the `gui` subsystem (no console). An
  embedded manifest sets common controls v6, per-monitor-v2 DPI,
  long-path awareness, UTF-8 code page and asInvoker, and adds version
  info.
* **Reproducible builds** (`tools/release.sh`):
  * the toolchain is pinned in `rust-toolchain.toml` (Rust 1.97.0), and CI
    and the release workflow install that exact version, so a new stable
    Rust can neither change release bytes nor break CI with new lints;
    bumping it is a deliberate change
  * flags: `--locked`, remapped path prefixes, no PE timestamp,
    `SOURCE_DATE_EPOCH` for archives
  * `--check-repro` builds twice in different directories; the three
    executables were byte-identical here
  * the MSI is not byte-reproducible (wixl writes a random package code);
    its product code is derived from the version
* **SBOM:** CycloneDX 1.5 JSON for the shipped binaries
  (`cargo-cyclonedx`).
* **Notices:** `THIRD-PARTY-NOTICES.txt` carries license files of the 65
  linked crates.
* **License gate:** `cargo deny check licenses` against a permissive
  allow-list (`deny.toml`), in CI and before every release build.
* **Signing:** Authenticode is a hook (`FLINT_SIGN_CMD`); without it the
  script labels each artifact unsigned. The update manifest is signed
  when `FLINT_UPDATE_SIGNING_KEY` is set.
* **CI** (`.github/workflows`):
  * tests and clippy on Linux and the Windows target
  * native Windows tests and the license gate
  * on `v*` tags, a reproducibility check and a draft GitHub release
  * CI passes on GitHub (Linux: 215 tests, clippy for both targets, Windows
    cross-build; native Windows: 213 tests; license gate). The release
    workflow has not run yet (it needs a `v*` tag)

## Open decisions needing the owner

1. **Elevation/service scope** (ADR-005). Without elevation, NTFS volumes
   are indexed by traversal plus change notifications, which is slower to
   build and re-walked at each start.
2. **Final project name.** "Flint" is a working name. The license is
   decided (ADR-013).
3. **Code signing** (Authenticode) for the executables and MSI. Free
   options for open-source projects exist (e.g. SignPath Foundation), as
   does Azure Trusted Signing. Wire it in through `FLINT_SIGN_CMD`.
4. **Update-signing key** (ADR-020): run `flint-release keygen` offline,
   add the public line to `crates/flint-update/update-keys.txt`, and store
   the secret as the `FLINT_UPDATE_SIGNING_KEY` repository secret. Until
   then builds cannot verify, and so never install, updates.

Resolved: reference PC and memory budget (ADR-013), license (ADR-013).
