# Benchmarks and measurement protocol

Targets are hypotheses to measure, not guarantees. Every result file in
`bench/results/` records the machine, toolchain and commands used.

## Status against targets

| Target | Measured so far | Where |
|---|---|---|
| Warm indexed search p50 ≤30 ms, p95 ≤100 ms | Linux VM, 4 vCPU. **1M entries:** first results p95 ≤15 ms (8/10 classes); completion p95 12–72 ms; p50 6–41 ms (exact-name, 2-edit and nonsense classes miss p50). **100k:** all p95 ≤8 ms | results/2026-10-19-linux-vm.md |
| Hotkey → interactive ≤100 ms | not measured on Windows (Wine figures are not valid) | spike prints it |
| UI input latency during indexing p95 ≤50 ms | not measured | spike prints keystroke → painted |
| Idle CPU ≤0.5% over 5 min | not measured | protocol below |
| Idle private bytes ≤ 25 MB + 70 B/entry (ADR-013; was ≤50 MB) | name table 68 MB at 1M synthetic entries after `shrink_to_fit` (was ~102 MB committed); process total not yet measured on Windows (the launcher prints private bytes when hidden; Wine reports 0) | results/2026-10-20-linux-vm-memory.md |
| Cached lightweight preview ≤50 ms | viewer not built | |

## Reference-PC protocol

Reference PC class (ADR-013): Windows 10 22H2 or 11, 4 cores, 8 GB RAM, SATA
SSD, 300k–1M files on the system volume, Balanced power plan, Defender on.

Record CPU, RAM, storage, Windows build, power plan, AV state and file
counts per volume.

1. **NTFS adapter validation:** run `ntfs_probe.exe C:` (elevated) and record
   enumeration time and record counts, the hard-link record behaviour, and
   journal read time.
2. **Hotkey → interactive:** run `flint-ui-win32.exe --synthetic 1000000`,
   press the hotkey 100 times with an input injector (e.g. AutoHotkey
   `SendInput`), and collect the printed `hotkey -> painted+focused` lines.
   Cross-check a sample with WPR/WPA (input event to first present).
3. **Input latency during indexing:** start the spike on a real user profile
   (traversal running), type a scripted query set with `--selftest`, and
   report keystroke → painted p50/p95.
4. **Idle CPU:** hide the window, wait 60 s, then sample process CPU for 300 s
   with `typeperf "\Process(flint-ui-win32)\% Processor Time" -si 1`. Report
   the mean.
5. **Idle private bytes:** same state; record `Private Bytes` from typeperf
   or Process Explorer, with the entry count.
6. **UI framework comparison (ADR-002):** repeat 2–5 for each framework
   prototype with an identical data set.
