# hex-upstream-v1's shards-api half: review (plan step 2, kiln goal #52, part 1)

Role: implementer, as ROOT's goal agent. Policy: origin/main 1145bd3f2, development-rules.md sha256 df2acf47…
Reviewed on main e38fbabb7: 41f4e3bff 9f3c91da4 037866828 1227113ec 3b90dc25c 1fd078bfb 84274050f e185bc998 ca94ecf8d
63ccdbdbe 301e5bddb 1b904c2f2 c80d14403 ca81077f5 ef3aa256d 195385791 63ec3449d 851452cef 5b4b22124 173fc9b4e 7c4e3c4ec.

## Fixed (red on main, green with the fix: jupiter, Windows 11 26200 x86_64; clippy -p shards-api --all-targets -D warnings clean there)

| Finding | Rule | Red | Green |
|---|---|---|---|
| `Shards::configured`'s `shards` got no `SystemRoot` (301e5bddb, Hex 0015): Winsock loads no provider, so every socket fails, the vsock's included (WSAEPROVIDERFAILEDINIT, 10106). Now the OS's own answer, `GetSystemWindowsDirectoryW`, as Hex's own Windows launches pass it | R01/R11 | `a_configured_providers_child_opens_sockets`: the child's bind panicked; the env test lacked it | both pass |
| `verify_native_image` (851452cef, Hex 0024) read an image 64 bytes at a time: a launch's `shards` binary and kernel (45.9 MB and 19.1 MB here) about a million reads. Now in the pieces its file system prefers (`st_blksize`; `FILE_STORAGE_INFO.PhysicalBytesPerSectorForPerformance`), SHA-256's block where it names none (D233) | R03/R08 | `a_verified_image_is_read_in_its_file_systems_pieces`: 16384 reads of a 1048576-byte image | under 16384 |

## Read and kept

The Windows report reader's overlapped reads (buffers and OVERLAPPED owned until completion, cancel then wait), the Unix
reader's poll (rounded-up milliseconds within the ABI, EINTR), the image check's TOCTOU guards, the probe's fallible
allocations, the drain's end mark (never empty, so `windows` never gets 0), refusals typed throughout.

## Owed to ROOT

- R03, not this lane's (8bd00b409, 171238a99; PM M463-M465): `shards_api::record::Learned` seeds test bounds from
  per-host-class measurements baked into the source. boot.rs's `E2E` is twice Linux-KVM's 1,206,822,260 ns, so CI's
  x86_64 KVM jobs at d35c4cf79 failed `shards_api_conforms_on_real_guests` ("no OK within 2.41364452s") and
  `shards_api_shares_a_directory_and_a_read_only_one` (`wait_for` gave `None`). conformance.rs's `OVERSHOOT` records
  and `UNSERVED_PATIENCE` are likewise. A bound learned where it runs, with its cold start designed, needs ROOT's
  decision and real VMs to show.
- Real VMs: `cargo test --profile test-release -p shards --test boot -- shards_api_` on HVF arm64, KVM x86_64,
  WHP x86_64, and KVM arm64 through the L1 path; WHP arm64 is unverified (no host). A boot through
  `Shards::configured` on WHP: no committed test boots through it (Hex's Windows launches do).
