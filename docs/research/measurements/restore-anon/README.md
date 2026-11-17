# A restore's anonymous memory on Intel (Envelope, `firecracker_restore/rss_anon`)

**Question.** CI's Firecracker comparison read shards' restored VM's anonymous memory
(`rss_anon`) worse than Firecracker's on an Intel Xeon 6975P-C runner, and better on AMD.
What does the restore hold on Intel that it does not on AMD?

**Which check.** The Envelope's `firecracker_restore` report, metric `rss_anon`
(scripts/bench-envelope): bounded by Firecracker's own paired samples, no allowance, as row
11 is. The release checklist has no row of its own for it.

**What CI's samples say** (the bench-samples artifacts of runs 44005213738 at b18952ae6 and
44020677971 at 8d6029e4d, n = 30 restores of each VMM, and their logs' mappings):

| | Intel Xeon 6975P-C | AMD EPYC 7763 |
|---|---|---|
| shards' `rss_anon`, median [min..max] | 1072 KiB [1072..1076] | 1048 KiB [1048..1052] |
| Firecracker's | 1048 KiB [1044..1060] | 1052 KiB [1044..1064] |
| Envelope, shards less Firecracker | +0.0253906 MiB, worse | -0.00390625 MiB, better |
| shards' guest pages written by its first beat (`written_pages`) | 232 in each of 30 | 226 in each of 30 |
| Firecracker's | 127 to 130 | 134 to 157 |
| the memory file's private copies in shards-vm's mappings | 946,176 B | 921,600 B |

vm-text's run 44031672087 (2893961893, AMD EPYC 9V74) read it better too: -0.0214844 MiB.

- The 24 KiB is the 6 pages more of guest RAM written on Intel, every restore alike: the
  memory file's private copies differ by 24,576 bytes, and shards-vm's own anonymous
  mappings (its stack, data and heap) hold no more on Intel.
- So it is no allocation of shards-vm's, its vCPU's XSAVE buffer among them, nor KVM's own:
  KVM's memory is the kernel's, or for its private memslots (VMX's APIC access page and
  TSS) shared anonymous memory, which `RssAnon` does not count. It is guest RAM written
  between the resume and the first beat: by the guest, by KVM into guest memory (kvmclock,
  steal time, PV EOI), or by shards-vm's devices (virtio rings). The restored guest has one
  vCPU on both hosts (the bench's `--cpus` 1, on runners of 4 CPUs).
- shards' restore path has no branch by CPU vendor (vmm: `cpuid::vendor` only reads one),
  so what differs is the guest's run on its host's CPU, as the snapshot's guest booted on
  it.

**What settles it** (ROOT's). Each restore writes the same pages every time, so one reading a
host names them: the comparison's `pages.tsv` (benches/firecracker.rs `pages_to`, beside each
run's samples) from one Intel 6975P-C run and one AMD run of the same revision, each restore's
privately copied pages of its memory file by offset. Then `written.py INTEL/pages.tsv
AMD/pages.tsv target/artifacts/vmlinux-6.18.48-x86_64-kvm-836568f8ad2c` lists the pages
written on one host and not the other, each in the kernel's image named by its symbols. CI's
firecracker job keeps `pages.tsv` in its `bench-samples-<sha>` artifact; the runner's CPU is
GitHub's choice, so a pair comes from the next runs that land on each.
