# Name-table memory, 2026-10-20 (Linux VM)

Machine and toolchain as in `2026-10-19-linux-vm.md` (4 vCPU VM, release
build, synthetic corpus seed 1).

Command:

```sh
cargo build --release -p flint-bench
./target/release/flint-bench engine --queries 20
```

| Measure (1,000,000 entries) | Before (phase 2) | After (phase 3) |
|---|---|---|
| Fixed columns per slot | 51 B | 43 B |
| Table as grown (capacity) | ~102 MB | 94.4 MB |
| Table after `shrink_to_fit` | — | **68.1 MB (68.1 B/entry)** |
| Plus chain-mask cache while searching | +8 MB | +8 MB (dropped when hidden) |

Capacity is what Windows commits as private bytes. Linux RSS undercounts it
(untouched capacity is not resident), so the earlier 74 MB RSS figure
understated the Windows cost.

The synthetic corpus has mostly lowercase names, so it rarely stores a
folded copy. Real Windows names are mostly mixed case and will cost more
name bytes; to be measured on the reference PC (ADR-013).
