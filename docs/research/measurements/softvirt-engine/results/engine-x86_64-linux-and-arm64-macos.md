# softvirt-engine: a translation engine's core mechanisms modelled in C (PM M523)

`engine.c` at b4f688be4, `cc -O2 -Wall -Wextra`, n=20000 batches of 256 operations a test
(5000 samples of 16 faults for the fault tests), reported per operation (per 16 faults),
2026-11-12, three runs on each host.

- x86_64: kiln runner kiln-a094a12d, Intel Core i9-14900HX, Fedora 44, Linux 6.19.10,
  GCC 16.2.1, `taskset -c 2` (a performance core), other jobs running (load 7.4 to 7.5).
- arm64: kiln runner adas-mac-studio.local-2, Apple M3 Ultra, macOS 26.4, Apple clang 21.0.0,
  unpinned, kiln's clamp (priority 20, nice 10), load 1.8 to 2.1. Its first run's native
  rows ran slower than the next two (warm-up, or an efficiency core); all three are kept.

## x86_64: run 1, run 2, run 3

| test | operation | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| load | native, 64 pages | 20000 | 0.6 | 0.6 | 0.6 | 22.3 |
| load | native, 256 MiB, random | 20000 | 10.9 | 12.3 | 32.0 | 85.6 |
| load | software MMU, TLB hit (64 pages) | 20000 | 1.2 | 1.3 | 1.7 | 11.7 |
| load | software MMU, miss and 4-level walk (256 MiB, random) | 20000 | 28.0 | 34.8 | 63.8 | 104.0 |
| switch | address-space switch, TLB tagged, then 16 pages touched | 20000 | 14.7 | 16.3 | 18.1 | 790.1 |
| switch | address-space switch, TLB flushed, then 16 pages touched | 20000 | 1748.4 | 1965.7 | 2206.6 | 4092.9 |
| syscall | native getppid | 20000 | 63.2 | 72.6 | 87.1 | 334.5 |
| syscall | translated guest system call, the engine's part | 20000 | 8.7 | 9.7 | 12.0 | 35.3 |
| fault | native, 16 first touches | 5000 | 9111.0 | 11219.0 | 14417.0 | 22973.0 |
| fault | in-engine, 16 first touches (zeroed frame, page tables built) | 5000 | 5458.0 | 6147.0 | 11566.0 | 20211.0 |
| load | native, 64 pages | 20000 | 0.6 | 0.6 | 0.8 | 13.9 |
| load | native, 256 MiB, random | 20000 | 11.0 | 11.9 | 32.6 | 101.8 |
| load | software MMU, TLB hit (64 pages) | 20000 | 1.2 | 1.3 | 1.6 | 15.4 |
| load | software MMU, miss and 4-level walk (256 MiB, random) | 20000 | 28.4 | 35.3 | 64.4 | 103.8 |
| switch | address-space switch, TLB tagged, then 16 pages touched | 20000 | 15.1 | 16.1 | 19.8 | 185.2 |
| switch | address-space switch, TLB flushed, then 16 pages touched | 20000 | 1769.2 | 1965.8 | 2180.9 | 2904.6 |
| syscall | native getppid | 20000 | 63.3 | 72.4 | 83.9 | 483.9 |
| syscall | translated guest system call, the engine's part | 20000 | 8.9 | 9.8 | 12.8 | 57.6 |
| fault | native, 16 first touches | 5000 | 11054.0 | 13221.0 | 17212.0 | 22683.0 |
| fault | in-engine, 16 first touches (zeroed frame, page tables built) | 5000 | 5782.0 | 6344.0 | 7171.0 | 23798.0 |
| load | native, 64 pages | 20000 | 0.6 | 0.7 | 0.8 | 14.9 |
| load | native, 256 MiB, random | 20000 | 11.7 | 13.2 | 31.3 | 385.0 |
| load | software MMU, TLB hit (64 pages) | 20000 | 1.5 | 2.3 | 3.0 | 20.4 |
| load | software MMU, miss and 4-level walk (256 MiB, random) | 20000 | 29.9 | 37.9 | 60.1 | 224.3 |
| switch | address-space switch, TLB tagged, then 16 pages touched | 20000 | 15.2 | 15.7 | 17.8 | 235.9 |
| switch | address-space switch, TLB flushed, then 16 pages touched | 20000 | 1801.4 | 1972.1 | 2290.4 | 3948.8 |
| syscall | native getppid | 20000 | 63.2 | 76.3 | 93.2 | 855.8 |
| syscall | translated guest system call, the engine's part | 20000 | 9.1 | 9.8 | 12.7 | 53.7 |
| fault | native, 16 first touches | 5000 | 9200.0 | 11483.0 | 16241.0 | 128838.0 |
| fault | in-engine, 16 first touches (zeroed frame, page tables built) | 5000 | 5885.0 | 6711.0 | 14874.0 | 20164.0 |

## arm64 (macOS): run 1, run 2, run 3

| test | operation | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| load | native, 64 pages | 20000 | 2.3 | 3.1 | 3.6 | 57.6 |
| load | native, 256 MiB, random | 20000 | 5.9 | 6.8 | 9.1 | 71.1 |
| load | software MMU, TLB hit (64 pages) | 20000 | 1.3 | 1.5 | 2.0 | 33.7 |
| load | software MMU, miss and 4-level walk (256 MiB, random) | 20000 | 11.7 | 13.7 | 19.2 | 71.8 |
| switch | address-space switch, TLB tagged, then 16 pages touched | 20000 | 33.9 | 41.6 | 46.9 | 963.6 |
| switch | address-space switch, TLB flushed, then 16 pages touched | 20000 | 682.2 | 708.4 | 919.2 | 2377.6 |
| syscall | native getppid | 20000 | 70.0 | 71.0 | 91.1 | 151.7 |
| syscall | translated guest system call, the engine's part | 20000 | 8.6 | 8.8 | 11.1 | 37.1 |
| fault | native, 16 first touches | 5000 | 2250.0 | 2625.0 | 5542.0 | 10333.0 |
| fault | in-engine, 16 first touches (zeroed frame, page tables built) | 5000 | 1625.0 | 1792.0 | 2500.0 | 12208.0 |
| load | native, 64 pages | 20000 | 0.5 | 0.6 | 0.7 | 35.5 |
| load | native, 256 MiB, random | 20000 | 2.9 | 3.3 | 4.1 | 45.2 |
| load | software MMU, TLB hit (64 pages) | 20000 | 1.1 | 1.5 | 1.8 | 25.6 |
| load | software MMU, miss and 4-level walk (256 MiB, random) | 20000 | 11.6 | 12.4 | 16.1 | 51.6 |
| switch | address-space switch, TLB tagged, then 16 pages touched | 20000 | 33.8 | 39.1 | 44.3 | 411.4 |
| switch | address-space switch, TLB flushed, then 16 pages touched | 20000 | 661.5 | 687.5 | 908.9 | 3078.1 |
| syscall | native getppid | 20000 | 70.0 | 70.6 | 89.7 | 169.8 |
| syscall | translated guest system call, the engine's part | 20000 | 8.6 | 8.8 | 11.1 | 43.0 |
| fault | native, 16 first touches | 5000 | 2250.0 | 2625.0 | 5458.0 | 13416.0 |
| fault | in-engine, 16 first touches (zeroed frame, page tables built) | 5000 | 1625.0 | 1791.0 | 2375.0 | 9000.0 |
| load | native, 64 pages | 20000 | 0.5 | 0.7 | 0.7 | 29.9 |
| load | native, 256 MiB, random | 20000 | 2.9 | 3.3 | 3.7 | 36.1 |
| load | software MMU, TLB hit (64 pages) | 20000 | 1.3 | 1.5 | 2.0 | 8.5 |
| load | software MMU, miss and 4-level walk (256 MiB, random) | 20000 | 11.7 | 12.5 | 16.8 | 44.4 |
| switch | address-space switch, TLB tagged, then 16 pages touched | 20000 | 36.4 | 41.7 | 49.5 | 523.4 |
| switch | address-space switch, TLB flushed, then 16 pages touched | 20000 | 679.7 | 692.8 | 877.6 | 2424.5 |
| syscall | native getppid | 20000 | 74.7 | 75.4 | 95.9 | 164.1 |
| syscall | translated guest system call, the engine's part | 20000 | 8.6 | 8.8 | 11.2 | 40.7 |
| fault | native, 16 first touches | 5000 | 2250.0 | 2542.0 | 5417.0 | 8916.0 |
| fault | in-engine, 16 first touches (zeroed frame, page tables built) | 5000 | 1625.0 | 1791.0 | 2458.0 | 14667.0 |
