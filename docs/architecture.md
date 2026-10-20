# Architecture (phase 4)

```
            ┌──────────────── UI thread (flint-ui-win32) ────────────────┐
 hotkey ──▶ │ edit box ─EN_CHANGE─▶ Session.submit(gen++) ──┐            │
            │ virtual ListView ◀─ PostMessage(Event{gen}) ◀─┼── stale gen │
            │ status line (coverage + search Status)        │   dropped   │
            └───────────────────────────────────────────────┼────────────┘
                                                            ▼
            ┌──────── search worker (flint-fuzzy::session) ─────────────┐
            │ debounce → cache (query, opts, table.generation)          │
            │ engine::search: Strong → Approx → Weak → Exhaustive       │
            │   rayon chunks, mask prefilters, top-K, CancelToken       │
            └───────────────▲───────────────────────────────────────────┘
                            │ RwLock<NameTable> (read)
            ┌───────────────┴──── indexer threads (flint-index) ────────┐
            │ VolumeIndexer per volume:                                 │
            │   validate(checkpoint, journal) → Resume | Rescan | Traverse | Wait
            │   enumerate / read journal → VolumeIndex (state machine)  │
            │   → follow-ups (links, meta, parents) → JournalSource     │
            │   → StoreOps + checkpoint in ONE SQLite transaction       │
            │ JournalSource: NtfsVolume (Win32) | TraversalSource | SimVolume
            └───────────────────────────────────────────────────────────┘
```

## Crates

| Crate | Purpose | Platform |
|---|---|---|
| `flint-fuzzy` | folding, kernels, `NameTable`, query parser, engine, session | any |
| `flint-index` | journal model, volume state machine, SQLite store, snapshots, catalog (per-volume workers), governor, traversal + reconciliation, content index (FTS5), deep search, NTFS simulator | any |
| `flint-ntfs` | USN and raw `$MFT` parsers (any); Win32 `NtfsVolume`, `DirWatcher` and `ntfs_probe` (Windows) | Windows for `win`, `watch` |
| `flint-preview` | wire protocol, preview/extraction backends, ZIP reader, limits (any); `flint-preview-host` helper, client with timeout/restart, viewer scheduler; shell thumbnails and Job Object (Windows) | any; `win` Windows |
| `flint-plugin` | manifest validation, approvals bound to a folder hash, length-prefixed JSON protocol, plugin process host (timeouts, backoff, strikes, output clamping), latest-wins service; low-integrity + Job Object spawn (Windows) | any; `win` Windows |
| `flint-plugin-epoch` | sample plugin and protocol reference | any |
| `flint-update` | ed25519-signed update manifests, version policy, verified downloads, `flint-release` maintainer tool; WinHTTP fetcher (Windows) | any; `win` Windows |
| `flint-ops` | file operations: plan/revalidate/execute/verify, path and name rules, compare, duplicates (any); `IFileOperation`, shell actions, clipboard, `IContextMenu` (Windows `win`) | Windows for `win` |
| `flint-commands` | command registry, `/s`, calculator | any |
| `flint-config` | versioned settings, migrations/recovery, data paths, hotkey parsing | any |
| `flint-ui-win32` | launcher UI; `model`, `ux`, `discovery` (any) + `win`, `actions`, `prompt`, `volumes`, `resources`, `theme`, `icons` (Windows) | Windows |
| `flint-bench` | engine, kernel, index benchmarks; corpus generator | any |

## Key invariants

* **Index honesty.** A volume is complete only in `Live` (journal caught up)
  or `Traversal{complete, skipped: 0}` state, with no runtime problem.
  `Enumerating` is persisted before enumeration starts.
* **Checkpoint safety.** Entries and checkpoint commit atomically, and
  application is idempotent, so replaying from any committed checkpoint
  converges.
* **Result stability.** Generations increase strictly. The UI applies only
  the latest generation and keeps the selected entry by id across partial
  and final results.
* **Re-entrancy.** Win32 dispatches messages synchronously inside many API
  calls. UI state uses fine-grained cells, and no borrow is held across such
  calls; a panic in the window procedure is caught, not unwound into user32.
* **Exhaustion scope.** `indexed_exhausted` refers to indexed candidates only;
  the UI adds index coverage separately.
* **No blind file operations.** Every destructive action is planned against
  the live filesystem, reviewed, revalidated immediately before acting and
  verified afterwards by observation (ADR-015).
* **Contents are read only in the helper.** Previews and content
  extraction run in `flint-preview-host` under timeouts and limits; the
  launcher process never parses file contents (ADR-016).
* **Indexing yields.** Worker threads run at background priority; one heavy
  scan runs at a time and pauses (boundedly) while the user types (ADR-014).

## Startup sequence

1. Resolve the data directory (installed or portable) and load the config.
   Load never fails; problems surface in the status line.
2. Create the window and register the configured hotkey; a taken hotkey is
   reported in the status line.
3. Start the catalog, then plan volumes on a background thread.
4. Each volume worker loads its snapshot (or falls back to SQLite), then
   syncs (journal), or walks and watches (traversal).
5. On exit, workers stop and write their snapshots.

## Threads at idle

UI thread (blocked in `GetMessage`), search worker (blocked on a channel),
the rayon pool (parked), and one catalog worker per volume, at background
priority:

* a journal volume polls once per second (an empty poll does no disk
  write)
* a watched volume blocks in `ReadDirectoryChangesW`

Idle CPU still has to be measured on Windows. `BytesToWaitFor` would remove
the journal poll entirely.
