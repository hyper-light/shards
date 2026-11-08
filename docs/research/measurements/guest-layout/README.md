# Guest memory's layout, against Firecracker (PM M157)

Where each VMM's guest memory is resident after a boot, 2 MiB by 2 MiB, and what shards'
layout cost in huge pages, on an x86_64 KVM host with THP. The layout this measured is
the one before PM M157's change, so the experiment applies to that revision: the parent
of the commit that adds this directory.

`layout-ab.patch` adds, there:

- to the x86_64 VMM, knobs: `SHARDS_INITRD=top` puts the initrd at the top of low RAM
  rather than at the first 2 MiB after the kernel; `SHARDS_LOW_4K=1` advises the first
  2 MiB of guest RAM MADV_NOHUGEPAGE;
- to the firecracker bench, `--layout-ab ARM,...`: arms `fc` (Firecracker), and shards as
  it is (`now`), `top`, `low4k` or `top-low4k`, interleaved, each order turned; every boot
  prints `ARM name`, then which 2 MiB stretches of guest memory its VMM holds and the
  4 KiB pages in each (`CHUNKS`, from /proc/PID/pagemap); and `SHARDS_BENCH_NO_THP=1`,
  which disables THP for both VMMs (PR_SET_THP_DISABLE), so that the counts are the
  pages the guest touched.

```
git checkout <this directory's commit>~ && git apply docs/research/measurements/guest-layout/layout-ab.patch
cargo bench -p shards --bench firecracker -- --layout-ab fc,now,top,low4k,top-low4k --runs 30 2> boots.log
SHARDS_BENCH_NO_THP=1 cargo bench -p shards --bench firecracker -- --layout-ab fc,now,top --runs 10 2> pages.log
python3 layout.py boots.log    # stretches held, per arm
python3 pages.py pages.log     # 4 KiB pages touched, per stretch
```

THP's own A/B needs no patch: `cargo bench -p shards --bench firecracker -- --thp-ab
all,never`.
