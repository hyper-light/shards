# softvirt-traps on GitHub's windows-11-arm runner, an arm64 cloud VM (PM M518)

Harness `windows.c`, `cl /nologo /O2 /W4` (the MSVC of the image's Visual Studio 2026),
workflow `softvirt-traps.yml`, runner image windows-11-vs2026-arm64 20261101.203.1:
Windows 11 Enterprise 10.0.26200, 4 processors, n=20000 a variant, 2026-11-12.

- Run 1: run 43135435859 (attempt 2), `windows.c` at 85f525fca. Its `debug` test printed
  nothing: its parent stepped every breakpoint forward, and Windows had already (see
  M518; fixed in df68484b0).
- Run 2: run 43162571571, `windows.c` at df68484b0.
- Run 3: run 43165501391, `windows.c` at 961409a6e, which names the CPU: Cobalt 100.
  Runs 1 and 2 did not record theirs.

## Run 1: run 43135435859

- QPC frequency: 10000000 Hz (100.0 ns a tick)
- processors: 4; page 4096 bytes; allocation granularity 65536 bytes

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | NtYieldExecution (each of 64 in a row) | 20000 | 104 | 106 | 107 | 773 |
| native | clock read alone | 20000 | 0 | 100 | 100 | 1900 |
| veh | breakpoint, vectored handler in-process | 20000 | 700 | 700 | 800 | 16300 |
| veh | access violation, handler VirtualProtects the page | 20000 | 1700 | 1700 | 1800 | 20600 |
| views | map a 64 KiB view into the child | 20000 | 800 | 900 | 2900 | 17800 |
| views | unmap it | 20000 | 1200 | 1200 | 1800 | 59800 |
| views | split a placeholder at 4 KiB | ok | | | | |
| views | a 4 KiB view at a 4 KiB offset | ok | | | | |
| views | a 4 KiB view at offset 0 | ok | | | | |
| jit | VirtualProtect read-write, then execute-read | 20000 | 700 | 800 | 800 | 16300 |
| jit | FlushInstructionCache of 8 bytes | 20000 | 200 | 200 | 200 | 3100 |

## Run 2: run 43162571571

- QPC frequency: 10000000 Hz (100.0 ns a tick)
- processors: 4; page 4096 bytes; allocation granularity 65536 bytes

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | NtYieldExecution (each of 64 in a row) | 20000 | 168 | 170 | 176 | 1214 |
| native | clock read alone | 20000 | 0 | 100 | 100 | 100 |
| veh | breakpoint, vectored handler in-process | 20000 | 1000 | 1000 | 1100 | 10000 |
| veh | access violation, handler VirtualProtects the page | 20000 | 2200 | 2300 | 2400 | 19100 |
| debug | breakpoint, debug event to the parent | 20000 | 27200 | 30600 | 36500 | 144600 |
| debug | breakpoints served | 0 with the PC at the instruction, 21001 past it; 0 other exceptions; exit 0 | | | | |
| views | map a 64 KiB view into the child | 20000 | 1000 | 1100 | 2200 | 31700 |
| views | unmap it | 20000 | 1400 | 1400 | 2100 | 51800 |
| views | split a placeholder at 4 KiB | ok | | | | |
| views | a 4 KiB view at a 4 KiB offset | ok | | | | |
| views | a 4 KiB view at offset 0 | ok | | | | |
| jit | VirtualProtect read-write, then execute-read | 20000 | 900 | 900 | 1000 | 32300 |
| jit | FlushInstructionCache of 8 bytes | 20000 | 300 | 300 | 300 | 11700 |

## Run 3: run 43165501391, Cobalt 100

- QPC frequency: 10000000 Hz (100.0 ns a tick)
- processors: 4; page 4096 bytes; allocation granularity 65536 bytes
- cpu: Cobalt 100

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | NtYieldExecution (each of 64 in a row) | 20000 | 170 | 173 | 215 | 3365 |
| native | clock read alone | 20000 | 0 | 100 | 100 | 100 |
| veh | breakpoint, vectored handler in-process | 20000 | 1000 | 1100 | 1100 | 42100 |
| veh | access violation, handler VirtualProtects the page | 20000 | 2200 | 2300 | 2400 | 103700 |
| debug | breakpoint, debug event to the parent | 20000 | 33100 | 38400 | 46800 | 197900 |
| debug | breakpoints served | 0 with the PC at the instruction, 21001 past it; 0 other exceptions; exit 0 | | | | |
| views | map a 64 KiB view into the child | 20000 | 1100 | 1100 | 2400 | 28000 |
| views | unmap it | 20000 | 1400 | 1500 | 2200 | 33100 |
| views | split a placeholder at 4 KiB | ok | | | | |
| views | a 4 KiB view at a 4 KiB offset | ok | | | | |
| views | a 4 KiB view at offset 0 | ok | | | | |
| jit | VirtualProtect read-write, then execute-read | 20000 | 900 | 1000 | 1000 | 23000 |
| jit | FlushInstructionCache of 8 bytes | 20000 | 300 | 300 | 400 | 5800 |
