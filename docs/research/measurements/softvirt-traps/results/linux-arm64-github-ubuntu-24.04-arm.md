# softvirt-traps on GitHub's ubuntu-24.04-arm runner, an arm64 cloud VM without /dev/kvm (PM M516)

Harness `linux.c`, `cc -O2 -Wall -Wextra -Werror` (GCC 13.3.0, Ubuntu
13.3.0-6ubuntu2~24.04.1), workflow `softvirt-traps.yml`, runner image ubuntu-24.04-arm
20261101.172.1: a 4-vCPU Azure VM on Arm Neoverse N2 (implementer 0x41, part 0xd49),
Linux 7.0.0-1017-azure, 15 GiB, load 0.27 to 0.68 as each run began, as the runner's
unprivileged user, n=20000 a variant, 2026-11-12. No `/dev/kvm`: no hardware
virtualization reaches this VM.

- Run 1: run 43135435859 (attempt 2), `linux.c` at 85f525fca. Its `maps` test crashed
  after meeting the map limit (see M516; fixed in df68484b0): no maps rows.
- Run 2: run 43162571571, `linux.c` at df68484b0.

## Run 1: run 43135435859

- kernel: Linux 7.0.0-1017-azure aarch64
- cpu implementer: 0x41
- cpu part: 0xd49
- x86 hypervisor flag: no
- dmi vendor: Microsoft Corporation
- dmi product: Virtual Machine
- /dev/kvm: absent
- cpus online: 4; measured on cpu 0, other core cpu 1
- clocksource: arch_sys_counter
- vm.max_map_count: 262144
- vm.unprivileged_userfaultfd: 0
- shmem THP: always within_size advise [never] deny force
- yama ptrace_scope: 1
- vulnerability spectre_v2: Mitigation: CSV2, BHB
- vulnerability indirect_target_selection: Not affected
- vulnerability itlb_multihit: Not affected
- vulnerability ghostwrite: Not affected
- vulnerability vmscape: Not affected
- vulnerability mmio_stale_data: Not affected
- vulnerability mds: Not affected
- vulnerability reg_file_data_sampling: Not affected
- vulnerability tsa: Not affected
- vulnerability l1tf: Not affected
- vulnerability spec_store_bypass: Mitigation: Speculative Store Bypass disabled via prctl
- vulnerability tsx_async_abort: Not affected
- vulnerability old_microcode: Not affected
- vulnerability spectre_v1: Mitigation: __user pointer sanitization
- vulnerability gather_data_sampling: Not affected
- vulnerability retbleed: Not affected
- vulnerability spec_rstack_overflow: Not affected
- vulnerability srbds: Not affected
- vulnerability meltdown: Not affected

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid, no filter | 20000 | 232 | 232 | 512 | 5440 |
| native | getppid, filter allows it | 20000 | 248 | 248 | 528 | 13033 |
| native | clock read alone | 20000 | 32 | 32 | 32 | 48 |
| sigsys | in-process handler | 20000 | 1352 | 1360 | 1640 | 22353 |
| futex | same CPU | 20000 | 2384 | 2552 | 3072 | 19352 |
| futex | other core | 20000 | 13969 | 14232 | 15888 | 893949 |
| futex | other core, spinning | 20000 | 328 | 336 | 344 | 54009 |
| handoff | SIGSYS + futex, same CPU | 20000 | 8248 | 9481 | 10417 | 26129 |
| handoff | SIGSYS + futex, other core | 20000 | 15968 | 16248 | 17960 | 739423 |
| handoff | SIGSYS + futex, other core, spinning | 20000 | 1568 | 1584 | 1856 | 26273 |
| notify | same CPU | 20000 | 4760 | 5129 | 5704 | 28913 |
| notify | same CPU, sync wake-up | 20000 | 4696 | 5896 | 6408 | 19968 |
| notify | other core | 20000 | 13505 | 13729 | 15064 | 657413 |
| notify | other core, sync wake-up | 20000 | 13601 | 13824 | 14729 | 76371 |
| sysemu | registers read and written, same CPU | 20000 | 4488 | 4728 | 5168 | 45265 |
| sysemu | registers read and written, other core | 20000 | 14505 | 14745 | 16113 | 772929 |
| fault | SIGSEGV, handler mprotects the page | 20000 | 1944 | 2120 | 2280 | 8529 |
| fault | SIGSEGV, handler maps a guest page (memfd) | 20000 | 3064 | 3344 | 4416 | 66538 |
| maps | 4 KiB pages at non-adjacent offsets | 262127 mapped of 263168; max_map_count 262144, 16 maps before; Cannot allocate memory | | | | |
| maps | test | failed | | | | |
| probe | RLIMIT_MEMLOCK (0 = unlimited) | soft 8192 KiB, hard 8192 KiB | | | | |
| probe | memfd_secret, 1 MiB mapped and touched | ok, first touch 2794 ns a page | | | | |
| probe | memfd_secret, 64 MiB mapped and touched | Resource temporarily unavailable | | | | |
| probe | userfaultfd, user-mode faults only | ok | | | | |
| probe | userfaultfd, all faults | Operation not permitted | | | | |
| probe | core scheduling cookie for this process | No such device | | | | |
| probe | Landlock ABI | 8 | | | | |

## Run 2: run 43162571571

- kernel: Linux 7.0.0-1017-azure aarch64
- cpu implementer: 0x41
- cpu part: 0xd49
- x86 hypervisor flag: no
- dmi vendor: Microsoft Corporation
- dmi product: Virtual Machine
- /dev/kvm: absent
- cpus online: 4; measured on cpu 0, other core cpu 1
- clocksource: arch_sys_counter
- vm.max_map_count: 262144
- vm.unprivileged_userfaultfd: 0
- shmem THP: always within_size advise [never] deny force
- yama ptrace_scope: 1
- vulnerability spectre_v2: Mitigation: CSV2, BHB
- vulnerability indirect_target_selection: Not affected
- vulnerability itlb_multihit: Not affected
- vulnerability ghostwrite: Not affected
- vulnerability vmscape: Not affected
- vulnerability mmio_stale_data: Not affected
- vulnerability mds: Not affected
- vulnerability reg_file_data_sampling: Not affected
- vulnerability tsa: Not affected
- vulnerability l1tf: Not affected
- vulnerability spec_store_bypass: Mitigation: Speculative Store Bypass disabled via prctl
- vulnerability tsx_async_abort: Not affected
- vulnerability old_microcode: Not affected
- vulnerability spectre_v1: Mitigation: __user pointer sanitization
- vulnerability gather_data_sampling: Not affected
- vulnerability retbleed: Not affected
- vulnerability spec_rstack_overflow: Not affected
- vulnerability srbds: Not affected
- vulnerability meltdown: Not affected

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid, no filter | 20000 | 232 | 240 | 520 | 18544 |
| native | getppid, filter allows it | 20000 | 248 | 248 | 528 | 18840 |
| native | clock read alone | 20000 | 32 | 32 | 32 | 48 |
| sigsys | in-process handler | 20000 | 1344 | 1360 | 1632 | 10288 |
| futex | same CPU | 20000 | 4000 | 4472 | 5184 | 28944 |
| futex | other core | 20000 | 13744 | 14040 | 20560 | 1498663 |
| futex | other core, spinning | 20000 | 240 | 248 | 264 | 32736 |
| handoff | SIGSYS + futex, same CPU | 20000 | 8656 | 9240 | 10392 | 51264 |
| handoff | SIGSYS + futex, other core | 20000 | 15520 | 15872 | 17832 | 3463375 |
| handoff | SIGSYS + futex, other core, spinning | 20000 | 1592 | 1608 | 1880 | 25144 |
| notify | same CPU | 20000 | 5632 | 5896 | 6592 | 73568 |
| notify | same CPU, sync wake-up | 20000 | 5352 | 5600 | 6224 | 60272 |
| notify | other core | 20000 | 13320 | 13568 | 14648 | 116768 |
| notify | other core, sync wake-up | 20000 | 13200 | 13432 | 14464 | 72496 |
| sysemu | registers read and written, same CPU | 20000 | 8832 | 9224 | 9840 | 67464 |
| sysemu | registers read and written, other core | 20000 | 14240 | 14512 | 15680 | 777151 |
| fault | SIGSEGV, handler mprotects the page | 20000 | 1928 | 1944 | 2000 | 16288 |
| fault | SIGSEGV, handler maps a guest page (memfd) | 20000 | 3000 | 3272 | 4392 | 71912 |
| maps | 4 KiB pages at non-adjacent offsets | 262127 mapped of 263168; max_map_count 262144, 16 maps before; Cannot allocate memory | | | | |
| maps | map one page, first 1000 | 1000 | 1528 | 2272 | 7008 | 17960 |
| maps | map one page, last 1000 | 1000 | 2224 | 2864 | 4856 | 183976 |
| maps | map one page, all | 262127 | 2040 | 2744 | 4744 | 204840 |
| maps | first touch of a mapped page | 262127 | 760 | 960 | 1216 | 63128 |
| maps | unmap one page | 262127 | 3416 | 4560 | 7952 | 301456 |
| maps | map one 2 MiB unit | 514 | 1544 | 2264 | 5064 | 14608 |
| maps | first touch of a 2 MiB unit's 512 pages | 514 | 312448 | 316448 | 335264 | 339256 |
| maps | unmap one 2 MiB unit | 514 | 60440 | 65648 | 88680 | 172655 |
| probe | RLIMIT_MEMLOCK (0 = unlimited) | soft 8192 KiB, hard 8192 KiB | | | | |
| probe | memfd_secret, 1 MiB mapped and touched | ok, first touch 3363 ns a page | | | | |
| probe | memfd_secret, 64 MiB mapped and touched | Resource temporarily unavailable | | | | |
| probe | userfaultfd, user-mode faults only | ok | | | | |
| probe | userfaultfd, all faults | Operation not permitted | | | | |
| probe | core scheduling cookie for this process | No such device | | | | |
| probe | Landlock ABI | 8 | | | | |
