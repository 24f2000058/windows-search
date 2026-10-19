# Architecture (phase 1)

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
| `flint-index` | journal model, volume state machine, SQLite store, traversal, NTFS simulator | any |
| `flint-ntfs` | USN record parser (any); Win32 `NtfsVolume` + `ntfs_probe` (Windows) | Windows for `win` |
| `flint-commands` | command registry, `/s`, calculator | any |
| `flint-ui-win32` | UI spike; `model` (any) + `win` (Windows) | Windows |
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

## Threads at idle

UI thread (blocked in `GetMessage`), search worker (blocked on a channel) and
the rayon pool (parked). Indexing threads exist only while work is pending;
journal polling cadence is a phase-2 decision (event-driven with
`BytesToWaitFor`, or a low-frequency timer).
