# softvirt-traps on macOS (PM M517)

Harness `macos.c` at 9d08a3ddd (SHA-256 16869d9a7c370e89…), `cc -O2 -Wall -Wextra`,
n=20000 a variant after 1000 warm-ups, 2026-11-12, kiln runner adas-mac-studio.local-2
(d1323898): Apple M3 Ultra (24 performance, 8 efficiency cores), macOS 26.4 (25E246),
Darwin 25.4.0, kern.hv_support 1, timebase 125/3 ns a tick, 16 KiB pages, load 1.8 to 2.0.
Kiln's commands run at priority 20, nice 10 (`ps -o pri,nice`): the utility clamp of an
agent's session. Three runs so; three of the hardened-runtime binary (`codesign -s - -f
-o runtime`, no entitlements); one through `launchd-run.sh` (a launchd job, ProcessType
Interactive: no clamp).

## Clamped (priority 20, nice 10): run 1, run 2, run 3

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid (each of 64 in a row) | 20000 | 78 | 151 | 221 | 765 |
| sigtrap | brk, in-process signal handler | 20000 | 3417 | 3625 | 4666 | 15833 |
| machexc | brk, Mach exception to a thread of the same process | 20000 | 4875 | 5709 | 10375 | 19791 |
| child-brk | brk, Mach exception to the parent | 20000 | 5375 | 6750 | 11125 | 25333 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 5542 | 7459 | 10875 | 19292 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 708 | 8917 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 958 | 6417 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 666 | 791 | 10333 |
| fault | signal, handler mprotects the page | 20000 | 4333 | 4542 | 6167 | 20792 |
| jit | write-protect off and on again | 20000 | 83 | 125 | 125 | 3958 |
| jit | write 2 instructions, invalidate, call | 20000 | 167 | 208 | 209 | 14292 |
| native | getppid (each of 64 in a row) | 20000 | 69 | 71 | 97 | 341 |
| sigtrap | brk, in-process signal handler | 20000 | 3417 | 3542 | 4583 | 17459 |
| machexc | brk, Mach exception to a thread of the same process | 20000 | 4917 | 5709 | 10250 | 17708 |
| child-brk | brk, Mach exception to the parent | 20000 | 5292 | 6167 | 10666 | 16625 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 5417 | 7209 | 11000 | 17292 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 625 | 6750 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 833 | 10625 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 625 | 792 | 7084 |
| fault | signal, handler mprotects the page | 20000 | 4292 | 4625 | 6208 | 16542 |
| jit | write-protect off and on again | 20000 | 83 | 125 | 125 | 334 |
| jit | write 2 instructions, invalidate, call | 20000 | 166 | 208 | 209 | 3334 |
| native | getppid (each of 64 in a row) | 20000 | 69 | 70 | 89 | 324 |
| sigtrap | brk, in-process signal handler | 20000 | 3417 | 3542 | 4459 | 12375 |
| machexc | brk, Mach exception to a thread of the same process | 20000 | 4667 | 5250 | 9875 | 16791 |
| child-brk | brk, Mach exception to the parent | 20000 | 5291 | 6416 | 10917 | 21125 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 5417 | 6750 | 11125 | 17834 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 667 | 6708 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 875 | 8209 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 666 | 834 | 7334 |
| fault | signal, handler mprotects the page | 20000 | 4333 | 4542 | 6250 | 20625 |
| jit | write-protect off and on again | 20000 | 83 | 125 | 125 | 292 |
| jit | write 2 instructions, invalidate, call | 20000 | 166 | 208 | 209 | 4708 |

Each run's task-port probe: the child sent its own task port; `task_for_pid` on it
failed; the parent received the port, allocated memory in the child, listed its 1 thread.

## Hardened runtime, no entitlements: run 1, run 2, run 3

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| child-brk | brk, Mach exception to the parent | 20000 | 5542 | 10167 | 25209 | 48583 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 5583 | 7250 | 11083 | 18500 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 667 | 4625 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 875 | 9291 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 666 | 750 | 9125 |
| child-brk | brk, Mach exception to the parent | 20000 | 5375 | 6375 | 10500 | 17500 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 5500 | 7167 | 10792 | 23375 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 541 | 667 | 7500 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 875 | 7834 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 666 | 792 | 7667 |
| child-brk | brk, Mach exception to the parent | 20000 | 5292 | 6333 | 10834 | 75084 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 5666 | 7959 | 11375 | 17417 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 709 | 8167 |
| taskport | mach_vm_deallocate it | 20000 | 708 | 750 | 958 | 6875 |
| taskport | thread_get_state of the child's thread | 20000 | 584 | 625 | 792 | 9875 |

Each run's task-port probe gave the same answers as the plain binary's.

## launchd job, ProcessType Interactive (no clamp)

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid (each of 64 in a row) | 20000 | 74 | 148 | 179 | 896 |
| sigtrap | brk, in-process signal handler | 20000 | 3417 | 3625 | 4125 | 18875 |
| machexc | brk, Mach exception to a thread of the same process | 20000 | 4875 | 5916 | 10375 | 26791 |
| child-brk | brk, Mach exception to the parent | 20000 | 5334 | 6334 | 10583 | 16417 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 5583 | 8208 | 10792 | 15959 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 667 | 6667 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 834 | 5583 |
| taskport | thread_get_state of the child's thread | 20000 | 584 | 625 | 750 | 9625 |
| fault | signal, handler mprotects the page | 20000 | 4333 | 4542 | 5875 | 18125 |
| jit | write-protect off and on again | 20000 | 83 | 125 | 125 | 209 |
| jit | write 2 instructions, invalidate, call | 20000 | 167 | 208 | 209 | 5500 |
