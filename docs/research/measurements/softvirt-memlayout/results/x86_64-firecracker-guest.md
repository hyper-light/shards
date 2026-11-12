# softvirt-memlayout: host mappings a guest's memory would need (PM M520)

kiln runner kiln-a094a12d (Intel Core i9-14900HX, Fedora 44, Linux 6.19.10), 2026-11-12. A
Firecracker v1.18.0 microVM on KVM: shards' pinned kernel 6.18.48, 2 vCPUs, 2 GiB, an
Alpine root (apk: busybox, musl, python3, nodejs, git). In it, `pagemap` (static, run as
root) walks every process's /proc/PID/maps and /proc/PID/pagemap: a run is a stretch of
resident pages whose virtual addresses and guest-physical frames both follow on.

## run.sh at e24270009: the same work under four huge-page policies, one boot

Work: python3 holding a dict of 1,000,000 entries and 200,000 64-byte objects; node holding
1,000,000 objects and 200,000 64-byte Buffers; counted 25 s after both start, then both
killed and the page cache dropped before the next policy. The guest's folio sizes: 8kB
16kB 32kB 64kB 128kB 256kB 512kB 1024kB 2048kB.

| policy | THP enabled | 64 KiB folios | AnonHugePages | process | guest mappings | resident pages | runs | runs within 2 MiB units |
|---|---|---|---|---|---|---|---|---|
| default | madvise | never | 0 kB | init | 18 | 240 | 237 | 237 |
| default | madvise | never | 0 kB | python3 | 43 | 41249 | 20725 | 20754 |
| default | madvise | never | 0 kB | node (MainThread) | 911 | 59839 | 16874 | 16885 |
| default | madvise | never | 0 kB | pagemap | 9 | 22 | 21 | 21 |
| thp-always | always | never | 73728 kB | init | 18 | 272 | 249 | 249 |
| thp-always | always | never | 73728 kB | python3 | 43 | 41304 | 17842 | 17854 |
| thp-always | always | never | 73728 kB | node (MainThread) | 897 | 58046 | 22674 | 22681 |
| thp-always | always | never | 73728 kB | pagemap | 9 | 20 | 14 | 14 |
| 64k-always | never | always | 0 kB | init | 18 | 272 | 249 | 249 |
| 64k-always | never | always | 0 kB | python3 | 43 | 41463 | 2858 | 2897 |
| 64k-always | never | always | 0 kB | node (MainThread) | 884 | 58665 | 9683 | 9691 |
| 64k-always | never | always | 0 kB | pagemap | 9 | 52 | 16 | 16 |
| all-always | always | always (every size) | 75776 kB | init | 18 | 272 | 249 | 249 |
| all-always | always | always (every size) | 75776 kB | python3 | 43 | 41532 | 1039 | 1055 |
| all-always | always | always (every size) | 75776 kB | node (MainThread) | 899 | 65307 | 3068 | 3079 |
| all-always | always | always (every size) | 75776 kB | pagemap | 9 | 54 | 21 | 21 |

| policy | guest mappings | resident pages | runs | pages a run | runs within 2 MiB units |
|---|---|---|---|---|---|
| default | 981 | 101350 (395 MiB) | 37857 | 2.7 | 37897 |
| thp-always | 967 | 99642 (389 MiB) | 40779 | 2.4 | 40798 |
| 64k-always | 954 | 100452 (392 MiB) | 12806 | 7.8 | 12853 |
| all-always | 969 | 107165 (418 MiB) | 4377 | 24.5 | 4404 |

## run.sh at dc05f8142: the first count, default policy only

Work: as above, with a git repository of 2,000 files committed first (git's processes gone
by the count). 4 processes with resident pages (init, python3, node, pagemap): 728 guest
mappings, 89,357 resident pages (349 MiB), 41,434 runs (2.2 pages a run), 41,500 within
2 MiB units; python3 41,247 pages in 21,302 runs, node 47,807 pages in 19,844 runs.
