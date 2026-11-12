# softvirt-traps on GitHub's ubuntu-24.04 runner, an x86_64 cloud VM (PM M516)

Harness `linux.c`, `cc -O2 -Wall -Wextra -Werror` (GCC 13.3.0, Ubuntu
13.3.0-6ubuntu2~24.04.1), workflow `softvirt-traps.yml`, runner image ubuntu-24.04
20261101.360.1: a 4-vCPU Azure VM, Linux 7.0.0-1017-azure, 15 GiB, load 0.18 to 0.49 as
each run began, as the runner's unprivileged user, n=20000 a variant, 2026-11-12. The two
runs drew different CPUs. `/dev/kvm` is present on this runner; the harness does not use it.

- Run 1: run 43135435859 (attempt 2), `linux.c` at 85f525fca, AMD EPYC 9V45. Its `maps`
  test crashed after meeting the map limit (see M516; fixed in df68484b0): no maps rows.
- Run 2: run 43162571571, `linux.c` at df68484b0, Intel Xeon Platinum 8573C.

## Run 1: run 43135435859, AMD EPYC 9V45

- kernel: Linux 7.0.0-1017-azure x86_64
- cpu: AMD EPYC 9V45 96-Core Processor
- x86 hypervisor flag: yes
- dmi vendor: Microsoft Corporation
- dmi product: Virtual Machine
- /dev/kvm: present
- cpus online: 4; measured on cpu 0, other core cpu 2
- clocksource: tsc
- vm.max_map_count: 262144
- vm.unprivileged_userfaultfd: 0
- shmem THP: always within_size advise [never] deny force
- yama ptrace_scope: 1
- vulnerability spectre_v2: Mitigation: Retpolines; STIBP: disabled; RSB filling; PBRSB-eIBRS: Not affected; BHI: Not affected
- vulnerability indirect_target_selection: Not affected
- vulnerability itlb_multihit: Not affected
- vulnerability ghostwrite: Not affected
- vulnerability vmscape: Not affected
- vulnerability mmio_stale_data: Not affected
- vulnerability mds: Not affected
- vulnerability reg_file_data_sampling: Not affected
- vulnerability tsa: Not affected
- vulnerability l1tf: Not affected
- vulnerability spec_store_bypass: Vulnerable
- vulnerability tsx_async_abort: Not affected
- vulnerability old_microcode: Not affected
- vulnerability spectre_v1: Mitigation: usercopy/swapgs barriers and __user pointer sanitization
- vulnerability gather_data_sampling: Not affected
- vulnerability retbleed: Not affected
- vulnerability spec_rstack_overflow: Vulnerable: Safe RET, no microcode
- vulnerability srbds: Not affected
- vulnerability meltdown: Not affected

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid, no filter | 20000 | 160 | 190 | 201 | 13179 |
| native | getppid, filter allows it | 20000 | 200 | 201 | 231 | 21232 |
| native | clock read alone | 20000 | 20 | 20 | 30 | 220 |
| sigsys | in-process handler | 20000 | 1803 | 1823 | 1843 | 33330 |
| futex | same CPU | 20000 | 5028 | 5278 | 6470 | 43916 |
| futex | other core | 20000 | 12909 | 13580 | 18768 | 460641 |
| futex | other core, spinning | 20000 | 110 | 121 | 140 | 27281 |
| handoff | SIGSYS + futex, same CPU | 20000 | 7402 | 7501 | 8823 | 31497 |
| handoff | SIGSYS + futex, other core | 20000 | 14542 | 15052 | 19910 | 617826 |
| handoff | SIGSYS + futex, other core, spinning | 20000 | 2063 | 2084 | 2413 | 12859 |
| notify | same CPU | 20000 | 5327 | 5378 | 6871 | 91717 |
| notify | same CPU, sync wake-up | 20000 | 5338 | 5418 | 6630 | 33210 |
| notify | other core | 20000 | 12308 | 13430 | 17537 | 2862419 |
| notify | other core, sync wake-up | 20000 | 11647 | 12299 | 118077 | 1500498 |
| sysemu | registers read and written, same CPU | 20000 | 9785 | 9945 | 13129 | 131557 |
| sysemu | registers read and written, other core | 20000 | 14602 | 15383 | 20772 | 1727679 |
| fault | SIGSEGV, handler mprotects the page | 20000 | 3245 | 3455 | 3676 | 17176 |
| fault | SIGSEGV, handler maps a guest page (memfd) | 20000 | 5248 | 5368 | 6609 | 142814 |
| maps | 4 KiB pages at non-adjacent offsets | 262120 mapped of 263168; max_map_count 262144, 24 maps before; Cannot allocate memory | | | | |
| maps | test | failed | | | | |
| probe | RLIMIT_MEMLOCK (0 = unlimited) | soft 8192 KiB, hard 8192 KiB | | | | |
| probe | memfd_secret, 1 MiB mapped and touched | ok, first touch 11206 ns a page | | | | |
| probe | memfd_secret, 64 MiB mapped and touched | Resource temporarily unavailable | | | | |
| probe | userfaultfd, user-mode faults only | ok | | | | |
| probe | userfaultfd, all faults | Operation not permitted | | | | |
| probe | core scheduling cookie for this process | ok | | | | |
| probe | Landlock ABI | 8 | | | | |

## Run 2: run 43162571571, Intel Xeon Platinum 8573C

- kernel: Linux 7.0.0-1017-azure x86_64
- cpu: INTEL(R) XEON(R) PLATINUM 8573C
- x86 hypervisor flag: yes
- dmi vendor: Microsoft Corporation
- dmi product: Virtual Machine
- /dev/kvm: present
- cpus online: 4; measured on cpu 0, other core cpu 2
- clocksource: tsc
- vm.max_map_count: 262144
- vm.unprivileged_userfaultfd: 0
- shmem THP: always within_size advise [never] deny force
- yama ptrace_scope: 1
- vulnerability spectre_v2: Mitigation: Retpolines; STIBP: disabled; RSB filling; PBRSB-eIBRS: Not affected; BHI: Retpoline
- vulnerability indirect_target_selection: Mitigation: Aligned branch/return thunks
- vulnerability itlb_multihit: Not affected
- vulnerability ghostwrite: Not affected
- vulnerability vmscape: Not affected
- vulnerability mmio_stale_data: Not affected
- vulnerability mds: Not affected
- vulnerability reg_file_data_sampling: Not affected
- vulnerability tsa: Not affected
- vulnerability l1tf: Not affected
- vulnerability spec_store_bypass: Vulnerable
- vulnerability tsx_async_abort: Not affected
- vulnerability old_microcode: Not affected
- vulnerability spectre_v1: Mitigation: usercopy/swapgs barriers and __user pointer sanitization
- vulnerability gather_data_sampling: Not affected
- vulnerability retbleed: Vulnerable
- vulnerability spec_rstack_overflow: Not affected
- vulnerability srbds: Not affected
- vulnerability meltdown: Not affected

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid, no filter | 20000 | 127 | 130 | 171 | 39424 |
| native | getppid, filter allows it | 20000 | 148 | 150 | 186 | 17768 |
| native | clock read alone | 20000 | 18 | 19 | 24 | 1514 |
| sigsys | in-process handler | 20000 | 1079 | 1101 | 1126 | 26380 |
| futex | same CPU | 20000 | 2279 | 2479 | 3196 | 18629 |
| futex | other core | 20000 | 24147 | 25128 | 33254 | 400005 |
| futex | other core, spinning | 20000 | 314 | 370 | 421 | 45634 |
| handoff | SIGSYS + futex, same CPU | 20000 | 5099 | 5509 | 6640 | 32088 |
| handoff | SIGSYS + futex, other core | 20000 | 24163 | 24971 | 31086 | 102857 |
| handoff | SIGSYS + futex, other core, spinning | 20000 | 1355 | 1421 | 1450 | 18063 |
| notify | same CPU | 20000 | 2428 | 2550 | 3031 | 20528 |
| notify | same CPU, sync wake-up | 20000 | 2505 | 2560 | 3418 | 37941 |
| notify | other core | 20000 | 24052 | 24803 | 30451 | 401780 |
| notify | other core, sync wake-up | 20000 | 23999 | 24715 | 29981 | 1382043 |
| sysemu | registers read and written, same CPU | 20000 | 4604 | 4655 | 4908 | 31838 |
| sysemu | registers read and written, other core | 20000 | 25419 | 26468 | 33479 | 2174925 |
| fault | SIGSEGV, handler mprotects the page | 20000 | 2041 | 2068 | 2160 | 28623 |
| fault | SIGSEGV, handler maps a guest page (memfd) | 20000 | 3218 | 3280 | 3463 | 142859 |
| maps | 4 KiB pages at non-adjacent offsets | 262120 mapped of 263168; max_map_count 262144, 24 maps before; Cannot allocate memory | | | | |
| maps | map one page, first 1000 | 1000 | 1506 | 2624 | 16010 | 23175 |
| maps | map one page, last 1000 | 1000 | 1647 | 2166 | 4705 | 8424 |
| maps | map one page, all | 262120 | 1589 | 2114 | 4761 | 236613 |
| maps | first touch of a mapped page | 262120 | 1091 | 1242 | 1443 | 33466 |
| maps | unmap one page | 262120 | 2523 | 3155 | 4653 | 180934 |
| maps | map one 2 MiB unit | 514 | 1508 | 2242 | 4010 | 17458 |
| maps | first touch of a 2 MiB unit's 512 pages | 514 | 412665 | 420148 | 448852 | 485645 |
| maps | unmap one 2 MiB unit | 514 | 40561 | 44130 | 62577 | 133605 |
| probe | RLIMIT_MEMLOCK (0 = unlimited) | soft 8192 KiB, hard 8192 KiB | | | | |
| probe | memfd_secret, 1 MiB mapped and touched | ok, first touch 18209 ns a page | | | | |
| probe | memfd_secret, 64 MiB mapped and touched | Resource temporarily unavailable | | | | |
| probe | userfaultfd, user-mode faults only | ok | | | | |
| probe | userfaultfd, all faults | Operation not permitted | | | | |
| probe | core scheduling cookie for this process | ok | | | | |
| probe | Landlock ABI | 8 | | | | |
