# The boot's anonymous memory against Firecracker's (Envelope, `firecracker/rss_anon`)

**Question.** The comparison's boot reads shards' `rss_anon` above Firecracker's by a median
of up to +0.64 MiB, worse in 2 of 4 runs on an Intel Xeon 6975P-C and 3 of 8 on an AMD EPYC
9V74, on both CPU vendors alike (PM M617). How much of it is huge pages' rounding, how much
pages shards' guest touches that Firecracker's does not, and how much the VMMs' own memory?

**What is known.** shards' guest memory is three mappings: its first 16 MiB, its kernel's
image's 24 MiB, which THP's boot advice holds in huge pages, every page resident in every
boot read, and the rest; Firecracker's is one, on base pages. CI's THP A/B (`--thp-ab
all,unadvised`) read `rss_anon` 0.1 to 0.2 MiB higher at p50 with the advice, and `to_ready`
46 to 50 ms lower (PM M157, M279, M617). No reading kept a boot's guest memory by page.

**The reading** (benches/firecracker.rs `guest_memory`, `write_guest`). Every boot of the
comparison, as its `rss_anon` is read, records its guest memory: over the mappings of guest
memory, their `Rss`, `AnonHugePages` and `Private_Dirty` (/proc/PID/smaps); each mapping's
span from guest memory's first byte, and its huge bytes; and the pages held in each huge
page's span of it (2 MiB under 4 KiB pages: /sys/kernel/mm/transparent_hugepage/
hpage_pmd_size), present and mapped by the VMM alone (/proc/PID/pagemap bits 63 and 56), as
`Rss` counts them. A row a boot, into `firecracker-guest.tsv` beside the samples
(SHARDS_BENCH_SAMPLES), which the envelope does not read. The boot's table says each VMM's
guest `Rss`, huge and dirty bytes and spans held (`*_guest_*`, unchecked).

**Taking the gap apart** (ROOT's). `attribute.py DIR...` over a CI run's `bench-samples`
artifact, or a vm-text run's output: for each pair of boots, the gap as `own` (each VMM's
anonymous memory outside its guest's), `rounding` (in each span one VMM holds in huge pages
and the other does not, the pages the other's guest left untouched there) and `touched` (the
rest, span by span), with what smaps' and pagemap's readings saw apart; each run's medians.
