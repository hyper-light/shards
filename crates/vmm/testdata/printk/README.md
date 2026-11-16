Rings of a real guest kernel's printk log, captured for `printk.rs`'s tests (D261) by
`docs/research/measurements/console-cost/capture-ring.py` from a snapshot of an x86-64 guest of
1 vCPU and 256 MiB (`shards run --kernel K --init testguest --cmdline "console=hvc0 panic=-1
shards_test=beat shards_snapshot=1" --snapshot-dir D`), on KVM (adacachy2), 2026-11-14:

- `ring-6.18.48-54fba99afaee.bin`: kernel-6.18.48-54fba99afaee, without patch 0015 (its console
  loses what was logged before it registered). SHA-256
  d1aaec5d8ddeb0dc2c80845ba4d86b28988e1369b6f69ebb89166218cfe96b9e.
- `ring-6.18.48-836568f8ad2c.bin`: kernel-6.18.48-836568f8ad2c, with it. SHA-256
  ae0e34f4025bd6770cfd1f3a1bd9ac21beb9bb5241e7edc89d67b4223c7989ef.

Each holds the VMCOREINFO page, `prb`, the ring's struct and its descriptor, info and text arrays
as runs of non-zero bytes at their guest-physical addresses (the format is in the script).
