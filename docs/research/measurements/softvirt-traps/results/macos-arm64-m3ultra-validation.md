# softvirt-traps on macOS (validation run, before PM M517's full runs)

Harness `macos.c` with the exception server's buffer fix (SHA-256 16869d9a7c370e89…,
committed after this run), `cc -O2 -Wall -Wextra`, n=2000 a variant, 2026-11-12, kiln
runner adas-mac-studio.local-2 (d1323898), run as kiln runs commands (its QoS not set by
the harness), load 1.76 1.85 1.83. Then the same binary ad-hoc signed with the hardened
runtime (`codesign -s - -f -o runtime`), with no entitlements.

- kernel: Darwin 25.4.0 arm64; macOS 26.4 (25E246)
- cpu: Apple M3 Ultra, 24 performance and 8 efficiency cores
- kern.hv_support 1, kern.hv_vmm_present 0
- timebase: 125/3 ns a tick (41.67 ns); page 16384 bytes

## Plain (ad-hoc signed by the linker)

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid (each of 64 in a row) | 2000 | 100 | 108 | 111 | 170 |
| sigtrap | brk, in-process signal handler | 2000 | 4166 | 4333 | 5292 | 18000 |
| machexc | brk, Mach exception to a thread of the same process | 2000 | 4917 | 5583 | 9708 | 18083 |
| child-brk | brk, Mach exception to the parent | 2000 | 6416 | 10083 | 11958 | 14750 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 2000 | 5917 | 8875 | 11750 | 13709 |
| taskport | mach_vm_map a 16 KiB page into the child | 2000 | 667 | 750 | 834 | 1417 |
| taskport | mach_vm_deallocate it | 2000 | 916 | 1000 | 1084 | 7041 |
| taskport | thread_get_state of the child's thread | 2000 | 625 | 833 | 917 | 3959 |
| fault | signal, handler mprotects the page | 2000 | 4542 | 6042 | 6500 | 17292 |
| jit | write-protect off and on again | 2000 | 83 | 125 | 125 | 167 |
| jit | write 2 instructions, invalidate, call | 2000 | 167 | 209 | 209 | 2917 |

The task-port probe: the child sent its own task port (`(os/kern) successful`);
`task_for_pid` on the child from the parent failed (`(os/kern) failure`); the parent
received the port, `mach_vm_allocate` in the child succeeded, `task_threads` gave 1 thread.

## Hardened runtime, no entitlements

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| child-brk | brk, Mach exception to the parent | 2000 | 10000 | 17333 | 21667 | 31959 |
| taskport | mach_vm_map a 16 KiB page into the child | 2000 | 750 | 834 | 1042 | 4000 |
| taskport | mach_vm_deallocate it | 2000 | 1000 | 1084 | 1375 | 1958 |
| taskport | thread_get_state of the child's thread | 2000 | 875 | 959 | 2334 | 15250 |
| jit | MAP_JIT | Invalid argument | | | | |

The task-port probe gave the same answers as the plain binary: sent, received, allocated in.
`MAP_JIT` is refused without `com.apple.security.cs.allow-jit`.
