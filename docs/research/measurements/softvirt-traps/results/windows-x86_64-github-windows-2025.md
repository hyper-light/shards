# softvirt-traps on GitHub's windows-2025 runner, an x64 cloud VM (PM M518)

Harness `windows.c`, `cl /nologo /O2 /W4` (the MSVC of the image's Visual Studio 2026),
workflow `softvirt-traps.yml`, runner image windows-2025-vs2026 20261103.298.1: Windows
Server 2025 Datacenter 10.0.26100, 4 processors, n=20000 a variant, 2026-11-12.

- Run 1: run 43135435859 (attempt 2), `windows.c` at 85f525fca. Its `debug` test printed
  nothing: its parent stepped every breakpoint forward, and Windows had already (see
  M518; fixed in df68484b0).
- Run 2: run 43162571571, `windows.c` at df68484b0.
- Run 3: run 43165501391, `windows.c` at 961409a6e, which names the CPU: an AMD EPYC 7763.
  Runs 1 and 2 did not record theirs.

## Run 1: run 43135435859

- QPC frequency: 10000000 Hz (100.0 ns a tick)
- processors: 4; page 4096 bytes; allocation granularity 65536 bytes

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | NtYieldExecution (each of 64 in a row) | 20000 | 187 | 189 | 282 | 4385 |
| native | clock read alone | 20000 | 0 | 100 | 100 | 52600 |
| veh | breakpoint, vectored handler in-process | 20000 | 1600 | 1600 | 1900 | 32900 |
| veh | access violation, handler VirtualProtects the page | 20000 | 3500 | 3500 | 6100 | 48800 |
| views | map a 64 KiB view into the child | 20000 | 1500 | 1600 | 3200 | 47700 |
| views | unmap it | 20000 | 1900 | 2000 | 3900 | 63900 |
| views | split a placeholder at 4 KiB | ok | | | | |
| views | a 4 KiB view at a 4 KiB offset | ok | | | | |
| views | a 4 KiB view at offset 0 | ok | | | | |
| jit | VirtualProtect read-write, then execute-read | 20000 | 1100 | 1200 | 1700 | 38100 |
| jit | FlushInstructionCache of 8 bytes | 20000 | 0 | 100 | 100 | 26600 |

## Run 2: run 43162571571

- QPC frequency: 10000000 Hz (100.0 ns a tick)
- processors: 4; page 4096 bytes; allocation granularity 65536 bytes

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | NtYieldExecution (each of 64 in a row) | 20000 | 151 | 164 | 179 | 573 |
| native | clock read alone | 20000 | 0 | 100 | 100 | 1700 |
| veh | breakpoint, vectored handler in-process | 20000 | 1200 | 1300 | 1700 | 26600 |
| veh | access violation, handler VirtualProtects the page | 20000 | 2900 | 3900 | 4400 | 48500 |
| debug | breakpoint, debug event to the parent | 20000 | 38000 | 47300 | 61800 | 270800 |
| debug | breakpoints served | 0 with the PC at the instruction, 21001 past it; 0 other exceptions; exit 0 | | | | |
| views | map a 64 KiB view into the child | 20000 | 1200 | 1300 | 2100 | 45500 |
| views | unmap it | 20000 | 1500 | 1500 | 2300 | 39100 |
| views | split a placeholder at 4 KiB | ok | | | | |
| views | a 4 KiB view at a 4 KiB offset | ok | | | | |
| views | a 4 KiB view at offset 0 | ok | | | | |
| jit | VirtualProtect read-write, then execute-read | 20000 | 800 | 900 | 1000 | 47900 |
| jit | FlushInstructionCache of 8 bytes | 20000 | 0 | 100 | 100 | 7000 |

## Run 3: run 43165501391, AMD EPYC 7763

- QPC frequency: 10000000 Hz (100.0 ns a tick)
- processors: 4; page 4096 bytes; allocation granularity 65536 bytes
- cpu: AMD EPYC 7763 64-Core Processor                

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | NtYieldExecution (each of 64 in a row) | 20000 | 185 | 187 | 267 | 1454 |
| native | clock read alone | 20000 | 0 | 100 | 100 | 100 |
| veh | breakpoint, vectored handler in-process | 20000 | 1600 | 1700 | 2700 | 36600 |
| veh | access violation, handler VirtualProtects the page | 20000 | 3400 | 5100 | 6100 | 88200 |
| debug | breakpoint, debug event to the parent | 20000 | 96800 | 110300 | 149500 | 11999800 |
| debug | breakpoints served | 0 with the PC at the instruction, 21001 past it; 0 other exceptions; exit 0 | | | | |
| views | map a 64 KiB view into the child | 20000 | 1600 | 2300 | 4200 | 886200 |
| views | unmap it | 20000 | 2000 | 2300 | 4700 | 1010100 |
| views | split a placeholder at 4 KiB | ok | | | | |
| views | a 4 KiB view at a 4 KiB offset | ok | | | | |
| views | a 4 KiB view at offset 0 | ok | | | | |
| jit | VirtualProtect read-write, then execute-read | 20000 | 1100 | 1600 | 2000 | 1156200 |
| jit | FlushInstructionCache of 8 bytes | 20000 | 0 | 100 | 100 | 24000 |
