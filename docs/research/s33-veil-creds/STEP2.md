# Step 33, step 2: the run's CA, in its network process and in its granted members' trust (D242)

Kiln goal #63 (8f474b45c7cd86559e0566f22f9d0c33), plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 2 of
the goal's plan 01a1d2b88be57304b972761e2a5c6781 (research 01a1d2b88be57304bf6b081fe46832e0: f1, f6, f8, f9, f11, f12,
f14, f37, f38, f57, f61, f62, f70). Policy: origin/main f64a75cae, AGENTS.md sha256 f66d304c…, development-rules.md
df2acf47…, development-evidence.md 61613a24…; role implementer (goal #63's agent).

## First slice: the run's CA in its network process (on main at 8479be870)

D242's "Built: step 2's first slice". Its focused run: kiln run r157 on adalundhe-macbook-pro (macOS arm64) at
96ac138dc: shards-net `ca::`/`credential` 5/5, shards-api `credential` 1/1, shards `api_run::tests::credential` 1/1,
clippy `-D warnings` of net, net-process, ipc, api and shards clean; shards-ipc's `every_message_kind_has_its_own_number`
red at the bare merge 5820551a8 (kind 96 taken twice) and green once `NET_CREDENTIALS` took 97.

## Second slice: the granted members' trust

D242's "Built: step 2's second slice". Paths: `crates/abi/src/run/trust.rs` (new), `crates/abi/src/run.rs`,
`crates/init/src/trust.rs` (new), `crates/init/src/{domains,build,proxyca,main}.rs`, `crates/shards/src/api_run.rs`.

- The run host adds `run-ca=<PEM>` and a `run-ca-host=<host>` per granted host to the run it hands its VM, once the
  network process answers the grants with the CA's DER; the grown spec is checked again (`spec::fits`).
- init checks both again; for each member whose link reaches a granted host (by name, or on any TCP port, as
  `shards_net::reaches` holds the run) lays its image's first trust bundle (`proxyca::BUNDLES`, found and read as the
  build does: build.rs `find_bundle`, `read_bundle`, now shared) with the CA appended between marker lines naming it,
  read-only over the bundle as its `/etc/hosts` is laid, and names the bundle by `NODE_EXTRA_CA_CERTS`,
  `SSL_CERT_FILE` and `CODEX_CA_CERTIFICATE`. Refused, never replaced: a member's own file of roots in one of those;
  a granted member with no `/tmp`; an image with no bundle. A member reaching no granted host is given nothing.

### Tests: one host, adalundhe-macbook-pro (macOS arm64), kiln run r222 at e1f1d1f0a, 987 s

The carrier (`.claude/worktrees/s33-carrier`: `run.sh`, `trust.sh`) clones Shards blobless on the runner, fetches the
bundled branch and runs `trust.sh`:

| Command | Result |
|---|---|
| `cargo test -p shards-abi trust` | 2 passed |
| `cargo test -p shards-init trust` (macOS: the portable tests) | 5 passed |
| `cargo test -p shards --bin shards api_run::tests::credential` | 2 passed (slice 1's and this slice's) |
| `cargo clippy -p shards-abi -p shards-init -p shards --all-targets -- -D warnings` | clean |
| `cargo clippy -p shards-init --target aarch64-unknown-linux-musl --all-targets -- -D warnings` | clean |
| `docker run … rust:1.99.1 cargo test -p shards-init -- trust:: proxy_ca bundle` (arm64 Linux, Docker Desktop on the same Mac; the image's toolchain, `RUSTUP_TOOLCHAIN=1.99.1`, not the pinned 1.98.1) | 12 passed: init's 6 `trust` tests, the Linux one included, and build.rs's CA-injection and bundle tests over the shared `find_bundle` |

R10, each guard removed once by hand in the runner's clone (perl), its test failing, then restored:

| Guard removed | Its test | Result |
|---|---|---|
| M1: any granted host's name grants a member (`n.eq_ignore_ascii_case(h)` → `true`) | `a_member_is_granted_where_its_link_reaches_a_granted_host` | red |
| M2: a member's own roots replaced (`if value != path` → `if false`) | `the_variables_name_the_bundle_and_a_members_own_roots_are_never_replaced` | red |
| M3: the bundle without the image's roots (`appended_between(original,` → `(b"",`) | `the_composed_bundle_holds_the_images_roots_then_the_ca` | red |
| M4: a second certificate taken (`… .is_some()` → `… .is_some() && false`) | `a_malformed_runs_ca_is_refused` | red |
| M5: PEM lines of 65 (`chunks(PEM_LINE)` → `chunks(PEM_LINE + 1)`) | `credential_grants_ca_goes_to_the_guest_in_pem_with_each_host_once` | red |

## Owed (ROOT)

- Real VMs (KVM, HVF, WHP): a credential-granted run whose granted member reads the run's CA at the end of its
  `/etc/ssl/certs/ca-certificates.crt`, read-only, with the three variables naming it, while an ungranted member of
  the same run reads its image's bundle unchanged and none of the three; on KVM x86_64, the network process making
  its CA under its seccomp filter (slice 1).
- Measured: the CA key's cost at the network process's start (slice 1), and `NODE_EXTRA_CA_CERTS` naming a whole
  bundle to Claude Code (its added roots read at start, and the request-body compression it turns off, f70) against a
  run without the CA.
- The other targets' clippy and tests: CI's matrix on main.
