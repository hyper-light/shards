# Hex's GPU patch 0008: the renderer holds the host's Vulkan driver loaded

**Defect (Hex's reproduction).** On Linux the render server serves each Venus context as a thread
of the GPU's VM process, and each context opens the host's Vulkan loader and makes an instance,
closing both as it ends. `host_visible` (virgl.rs) made the renderer's first instance and
destroyed it, closing the loader, so nothing held the host's driver between contexts: each
context loaded it anew. NVIDIA's libnvidia-tls is DF_STATIC_TLS. Hex reproduced it on adacachy2
(00aad, RTX 5060 Ti, driver 620.70.06, a VM of 2 vCPUs and 1 GiB): the guest program's 19th run
in one VM failed at vkEnumeratePhysicalDevices (VK_ERROR_INITIALIZATION_FAILED), 3 of 3 VMs and
Hex's census, the host loader saying `libnvidia-tls.so.620.70.06: cannot allocate memory in
static TLS block`, then `Failed to detect any valid GPUs`; with the hold, 200 of 200 ran
(Hex's vendor/shards-gpu/UPSTREAM.md and patch 0008). Shards' shared Venus path has the same
`host_visible`: the defect is shards' own, not only Hex's selection line's.

**Fix (Hex's 0008, adapted to shards' `host_visible()`, which takes no selection here).** The
instance `host_visible` makes, with its loader, is kept as `HeldDriver` (instance destroyed as it
drops, the loader closed after) and held by `Venus` as its last field, dropped after the renderer.

**Red/green (R09/R10).** `held-driver.sh held-driver-g0008 5` on adacachy (451397fc, CachyOS,
kernel 7.2.9-2-cachyos, glibc 2.44, NVIDIA GeForce RTX 5060 Ti, driver 615.78.08, ICDs
nvidia_icd.json and radeon_icd.json), 2026-11-11, base 03786682, renderer libraries from
`scripts/gpu-libs` (libvirglrenderer.a d5376d00…, librender_server.a 41bdc2c1…, libepoxy.a
59d21e5a…), MemoryMax=24G, -j4. Hex's test,
`a_contexts_instance_loads_no_static_tls_driver_anew_while_the_renderer_holds_it`, keeps what
`host_visible` returns and makes a context's instance as a render-server context does, then
lists the loaded objects whose DT_FLAGS has DF_STATIC_TLS:

| Build | virgl.rs | Runs passed | What failed |
|---|---|---|---|
| red: production as committed, Hex's test (g0008-tests-only.patch) | 612e5cc0… | 0 of 5 | each run: `a context's instance loaded ["/usr/lib/libGLX_nvidia.so.0", "/usr/lib/libnvidia-glcore.so.615.78.08", "/usr/lib/libnvidia-tls.so.615.78.08"] anew` |
| green: the driver held | 471f7666… | 5 of 5 | none |
| red again (`held-driver-g0008-final`) | 612e5cc0… | 0 of 5 | the same three objects, each run |
| green, its comments' account of glibc corrected | d202722c… | 5 of 5 | none |

The committed virgl.rs is the last green's plus rustfmt's line breaks in two test statements.

The renderer's own test (`the_renderer_offers_venus_and_holds_its_blobs`: Venus starts, offers
its capability set, makes a context, holds and maps a blob) passes with the held driver on the
same GPU. The red build is the guard removed: production without the hold. `Venus`'s
`_driver` field is typed `HeldDriver`, which only `host_visible`'s held instance makes, so the
renderer cannot start without holding one.

**The exhaustion itself, host-only: not reproduced.** `static-tls-loop.c` repeats a context's
calls (open the loader, make an instance, list GPUs, destroy it, close the loader) in a bare C
process: 60 rounds listed the GPU every time, unheld and held alike (`VK_LOADER_DEBUG=error`
silent). glibc gives back a closed module's static TLS where it ends the used area
(`_dl_close_worker`'s tls_free_start/tls_free_end), as a lone loader and ICD's does each round;
why the GPU's VM process does not get it back (Hex's 3 of 3) is not established here. That end
symptom, the 19th context's failure, needs a GPU VM: owed to ROOT (a GPU guest running a
Vulkan program 30 times in one VM on an NVIDIA Linux host, before and after this change).

**Raw.** On adacachy: `~/g121-tmp/hex-upstream-v1/archive-held-driver-g0008.tgz` (sha256
c7acf1b96d2e54c1a7867355676be11be7bec7f92cfc1403ab6363bd1c1e06ea: identity, every red and green
log, the renderer test's log, both virgl.rs builds, the loop's logs).
