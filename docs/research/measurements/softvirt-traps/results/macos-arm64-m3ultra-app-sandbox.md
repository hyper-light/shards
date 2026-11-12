# softvirt-traps on macOS under App Sandbox (PM M522)

Harness `macos.c` at a5e6b162f (the task port handed over through the child's exception
port), `cc -O2 -Wall -Wextra`, an Info.plist (CFBundleIdentifier dev.shards.softvirt-traps)
in `__TEXT,__info_plist`, signed `codesign -s - -f -o runtime` with
`com.apple.security.app-sandbox` and `com.apple.security.cs.allow-jit`. Parent and child
both run in App Sandbox (a child of a sandboxed process inherits it). n=20000 a variant,
2026-11-12, kiln runner adas-mac-studio.local-2 (Apple M3 Ultra, macOS 26.4), kiln's
clamp (priority 20, nice 10), load 1.9 to 2.0. First, the same build unsandboxed (taskport
only), then three sandboxed runs.

Without the Info.plist, the sandboxed binary died at launch (SIGTRAP, "Trace/BPT trap: 5")
before printing, as crates/vm-process/build.rs says a sandboxed tool without one does.
With the earlier harness (9d08a3ddd), which handed the task port over the bootstrap port,
the sandboxed child never sent it: the parent received a message of another size on that
port ("(ipc/rcv) msg too large") and no task port.

## Unsandboxed

- child: sending its own task port: (os/kern) successful
- parent: task_for_pid(child): (os/kern) failure
- parent: receiving the child's task port: (os/kern) successful (id 0, 40 bytes)
- parent: mach_vm_allocate in the child: (os/kern) successful; task_threads: 1 thread

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 667 | 792 | 917 | 5875 |
| taskport | mach_vm_deallocate it | 20000 | 916 | 1084 | 1208 | 6916 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 708 | 750 | 12084 |

## App Sandbox, hardened runtime, allow-jit: run 1, run 2, run 3

Each run's probe: the child sent its own task port; task_for_pid failed; the parent
received the port (id 0, 40 bytes), allocated in the child, listed its 1 thread.

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| child-brk | brk, Mach exception to the parent | 20000 | 5666 | 8792 | 11792 | 20500 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 6708 | 9125 | 11500 | 18667 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 625 | 10084 |
| taskport | mach_vm_deallocate it | 20000 | 708 | 750 | 833 | 7709 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 667 | 833 | 9666 |
| jit | write-protect off and on again | 20000 | 83 | 125 | 125 | 334 |
| jit | write 2 instructions, invalidate, call | 20000 | 125 | 167 | 209 | 5666 |
| child-brk | brk, Mach exception to the parent | 20000 | 5417 | 7750 | 11000 | 33625 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 6792 | 9541 | 11583 | 21959 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 667 | 6542 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 875 | 6875 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 667 | 875 | 10125 |
| jit | write-protect off and on again | 20000 | 83 | 125 | 125 | 4125 |
| jit | write 2 instructions, invalidate, call | 20000 | 167 | 208 | 209 | 4875 |
| child-brk | brk, Mach exception to the parent | 20000 | 5500 | 8375 | 10916 | 20709 |
| child-fault | load of a PROT_NONE page, Mach exception to the parent | 20000 | 6791 | 9000 | 11500 | 19750 |
| taskport | mach_vm_map a 16 KiB page into the child | 20000 | 500 | 542 | 625 | 7083 |
| taskport | mach_vm_deallocate it | 20000 | 667 | 709 | 833 | 8417 |
| taskport | thread_get_state of the child's thread | 20000 | 625 | 667 | 709 | 7916 |
| jit | write-protect off and on again | 20000 | 83 | 125 | 125 | 3833 |
| jit | write 2 instructions, invalidate, call | 20000 | 167 | 209 | 250 | 9667 |
