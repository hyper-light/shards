# softvirt-traps on bare-metal x86_64 Linux (PM M516)

Harness `linux.c` at 27e1406a1, `cc -O2 -Wall -Wextra -Werror` (GCC 16.2.1), as an
unprivileged user, 2026-11-12, three runs of n=20000 one after another on kiln runner
kiln-a094a12d (8fadc1ec), with other jobs on the host (load 7.15 6.93 6.96, 32 CPUs).
The probes are from 85f525fca's `probe` test, the same day, same host.

- kernel: Linux 6.19.10-300.fc44.x86_64 x86_64
- cpu: Intel(R) Core(TM) i9-14900HX; x86 hypervisor flag: no; /dev/kvm: present (unused)
- cpus online: 32; measured on cpu 0, other core cpu 2
- clocksource: tsc
- vm.max_map_count: 1048576; vm.unprivileged_userfaultfd: 0; shmem THP: never
- yama ptrace_scope: 0
- vulnerability spectre_v2: Enhanced / Automatic IBRS; IBPB: conditional; PBRSB-eIBRS: SW sequence; BHI: BHI_DIS_S
- vulnerability vmscape: IBPB before exit to userspace; reg_file_data_sampling: Clear Register File
- vulnerability spec_store_bypass: disabled via prctl; spectre_v1: usercopy/swapgs barriers
- every other vulnerability file: Not affected

## Run 1

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid, no filter | 20000 | 91 | 94 | 106 | 670440 |
| native | getppid, filter allows it | 20000 | 131 | 142 | 164 | 3907 |
| native | clock read alone | 20000 | 16 | 18 | 20 | 370 |
| sigsys | in-process handler | 20000 | 747 | 930 | 1066 | 17815 |
| futex | same CPU | 20000 | 1775 | 2154 | 3751 | 9441 |
| futex | other core | 20000 | 2879 | 3431 | 6362 | 741718 |
| futex | other core, spinning | 20000 | 108 | 139 | 164 | 5998 |
| handoff | SIGSYS + futex, same CPU | 20000 | 2695 | 3062 | 5465 | 21451 |
| handoff | SIGSYS + futex, other core | 20000 | 3587 | 4278 | 7289 | 136162 |
| handoff | SIGSYS + futex, other core, spinning | 20000 | 821 | 900 | 1094 | 9864 |
| notify | same CPU | 20000 | 1832 | 2246 | 4001 | 21308 |
| notify | same CPU, sync wake-up | 20000 | 1912 | 2247 | 4096 | 15215 |
| notify | other core | 20000 | 2786 | 3497 | 6116 | 552447 |
| notify | other core, sync wake-up | 20000 | 2804 | 3332 | 6479 | 1005175 |
| sysemu | registers read and written, same CPU | 20000 | 3598 | 4933 | 6978 | 95488 |
| sysemu | registers read and written, other core | 20000 | 3673 | 4342 | 7376 | 190029 |
| fault | SIGSEGV, handler mprotects the page | 20000 | 1105 | 1308 | 1898 | 8076 |
| fault | SIGSEGV, handler maps a guest page (memfd) | 20000 | 1832 | 2092 | 4900 | 165659 |
| maps | 4 KiB pages at non-adjacent offsets | 263168 mapped of 263168; max_map_count 1048576, 22 maps before; no failure | | | | |
| maps | map one page, first 1000 | 1000 | 718 | 1320 | 4835 | 12945 |
| maps | map one page, last 1000 | 1000 | 1048 | 1485 | 4692 | 10389 |
| maps | map one page, all | 263168 | 1020 | 1597 | 4753 | 310452 |
| maps | first touch of a mapped page | 263168 | 618 | 807 | 1108 | 155708 |
| maps | unmap one page | 263168 | 844 | 1272 | 4238 | 2029775 |
| maps | map one 2 MiB unit | 514 | 704 | 1211 | 3035 | 6987 |
| maps | first touch of a 2 MiB unit's 512 pages | 514 | 265965 | 292604 | 333432 | 2224976 |
| maps | unmap one 2 MiB unit | 514 | 37916 | 44007 | 52223 | 68279 |

## Run 2

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid, no filter | 20000 | 63 | 75 | 78 | 192891 |
| native | getppid, filter allows it | 20000 | 79 | 88 | 98 | 2266 |
| native | clock read alone | 20000 | 10 | 11 | 12 | 1925 |
| sigsys | in-process handler | 20000 | 700 | 759 | 987 | 20759 |
| futex | same CPU | 20000 | 1878 | 2199 | 3866 | 15167 |
| futex | other core | 20000 | 2854 | 3521 | 7881 | 997509 |
| futex | other core, spinning | 20000 | 145 | 171 | 206 | 4620 |
| handoff | SIGSYS + futex, same CPU | 20000 | 2645 | 3055 | 5014 | 26465 |
| handoff | SIGSYS + futex, other core | 20000 | 3489 | 4142 | 7176 | 234845 |
| handoff | SIGSYS + futex, other core, spinning | 20000 | 832 | 930 | 1016 | 12399 |
| notify | same CPU | 20000 | 1972 | 2230 | 3932 | 20880 |
| notify | same CPU, sync wake-up | 20000 | 1808 | 2195 | 4053 | 24747 |
| notify | other core | 20000 | 2848 | 3316 | 6193 | 152684 |
| notify | other core, sync wake-up | 20000 | 2837 | 3314 | 6168 | 208091 |
| sysemu | registers read and written, same CPU | 20000 | 3493 | 4046 | 6633 | 98902 |
| sysemu | registers read and written, other core | 20000 | 3678 | 4358 | 7291 | 636670 |
| fault | SIGSEGV, handler mprotects the page | 20000 | 1253 | 1322 | 1980 | 19696 |
| fault | SIGSEGV, handler maps a guest page (memfd) | 20000 | 1824 | 2612 | 4913 | 269852 |
| maps | 4 KiB pages at non-adjacent offsets | 263168 mapped of 263168; max_map_count 1048576, 22 maps before; no failure | | | | |
| maps | map one page, first 1000 | 1000 | 695 | 1215 | 4067 | 9108 |
| maps | map one page, last 1000 | 1000 | 1323 | 1994 | 4833 | 144142 |
| maps | map one page, all | 263168 | 1013 | 1583 | 4419 | 1432496 |
| maps | first touch of a mapped page | 263168 | 613 | 783 | 1044 | 29383 |
| maps | unmap one page | 263168 | 850 | 1295 | 4248 | 2053599 |
| maps | map one 2 MiB unit | 514 | 793 | 1290 | 4045 | 6492 |
| maps | first touch of a 2 MiB unit's 512 pages | 514 | 259747 | 293330 | 356978 | 1578422 |
| maps | unmap one 2 MiB unit | 514 | 40657 | 46766 | 52153 | 59163 |

## Run 3

| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |
|---|---|---|---|---|---|---|
| native | getppid, no filter | 20000 | 76 | 81 | 94 | 3654 |
| native | getppid, filter allows it | 20000 | 82 | 90 | 127 | 2644 |
| native | clock read alone | 20000 | 11 | 13 | 14 | 97 |
| sigsys | in-process handler | 20000 | 699 | 801 | 1195 | 654899 |
| futex | same CPU | 20000 | 1790 | 2161 | 3709 | 102618 |
| futex | other core | 20000 | 2957 | 3466 | 6300 | 154536 |
| futex | other core, spinning | 20000 | 152 | 200 | 248 | 5691 |
| handoff | SIGSYS + futex, same CPU | 20000 | 2560 | 2912 | 5064 | 18701 |
| handoff | SIGSYS + futex, other core | 20000 | 3866 | 4761 | 8133 | 980204 |
| handoff | SIGSYS + futex, other core, spinning | 20000 | 879 | 984 | 1152 | 10128 |
| notify | same CPU | 20000 | 1863 | 2226 | 3984 | 22883 |
| notify | same CPU, sync wake-up | 20000 | 1776 | 2129 | 3504 | 22546 |
| notify | other core | 20000 | 2868 | 3338 | 6158 | 707762 |
| notify | other core, sync wake-up | 20000 | 2857 | 3312 | 6509 | 996316 |
| sysemu | registers read and written, same CPU | 20000 | 3497 | 4069 | 6585 | 114454 |
| sysemu | registers read and written, other core | 20000 | 3648 | 4329 | 7279 | 1006625 |
| fault | SIGSEGV, handler mprotects the page | 20000 | 1205 | 1347 | 2087 | 9874 |
| fault | SIGSEGV, handler maps a guest page (memfd) | 20000 | 1736 | 2027 | 4849 | 18024 |
| maps | 4 KiB pages at non-adjacent offsets | 263168 mapped of 263168; max_map_count 1048576, 22 maps before; no failure | | | | |
| maps | map one page, first 1000 | 1000 | 755 | 1170 | 4428 | 7061 |
| maps | map one page, last 1000 | 1000 | 983 | 1453 | 5257 | 14047 |
| maps | map one page, all | 263168 | 989 | 1507 | 4276 | 1099226 |
| maps | first touch of a mapped page | 263168 | 620 | 808 | 1118 | 907899 |
| maps | unmap one page | 263168 | 843 | 1259 | 4197 | 1390798 |
| maps | map one 2 MiB unit | 514 | 764 | 1256 | 3627 | 4390 |
| maps | first touch of a 2 MiB unit's 512 pages | 514 | 270318 | 295219 | 341123 | 1299489 |
| maps | unmap one 2 MiB unit | 514 | 38080 | 44633 | 52209 | 68028 |

## Probes (85f525fca)

| test | variant | result |
|---|---|---|
| probe | RLIMIT_MEMLOCK | soft 8192 KiB, hard 8192 KiB |
| probe | memfd_secret, 1 MiB mapped and touched | ok, first touch 13754 ns a page |
| probe | memfd_secret, 64 MiB mapped and touched | Resource temporarily unavailable |
| probe | userfaultfd, user-mode faults only | ok |
| probe | userfaultfd, all faults | Operation not permitted |
| probe | core scheduling cookie for this process | ok |
| probe | Landlock ABI | 7 |
