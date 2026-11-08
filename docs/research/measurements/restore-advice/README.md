# Which advice keeps a restored guest's memory file small (Firecracker comparison)

After a restore, shards has 19 MB of the snapshot's memory file resident where Firecracker,
restoring the same guest, has 10 MB (M53's comparison run, ccf5bbc: 18.5 against 11.6 MiB
of `Pss_File`). `advice.patch` lets `SHARDS_FILE_ADVICE` choose the madvise(2) the private
file mapping of a restore gets: `random` (no readahead), `sequential`, `nohuge`, `huge`, or
none. `.github/workflows/restore-advice.yml` runs the Firecracker comparison once for each,
on CI's KVM runner, and prints each run's restore rows.
