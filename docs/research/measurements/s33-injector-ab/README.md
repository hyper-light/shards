# s33-injector-ab: what the credential injector costs

#74 step 10 part B (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 33; D242). The harness is
`ab.rs`. It is compiled into shards-net's unit tests as `inject::ab`, as ignored tests. It drives
the injector (`crates/net/src/inject.rs`) in-process, as step 3's tests do, so it needs no VM and
runs on any host shards-net builds on.

## Running it

From a checkout at the revision being measured:

```sh
S33_AB_OUT=docs/research/measurements/s33-injector-ab/results/<host>-<revision> \
  cargo test -p shards-net --release --lib inject::ab -- --ignored --nocapture --test-threads=1
```

- Use `--release`. Measurements keep `release` (CLAUDE.md), and a build with debug assertions
  heads its report `NOT A MEASUREMENT`.
- On macOS, run it through `docs/research/measurements/macos-sched/launchd-run.sh OUT env S33_AB_OUT=… cargo test …`.
  An agent's session runs under a utility QoS clamp (PM M317). launchd-run.sh passes on only
  `PATH`, `HOME` and `TMPDIR`, so set the `S33_AB_*` variables with `env` inside its command.
- Under load: run it again with `S33_AB_LOAD=cpus`, which keeps one busy thread on each of the
  host's CPUs (`available_parallelism`) for the whole run, or `S33_AB_LOAD=N` for N threads.
  You can also run it beside the host's real work. It never waits for quiet. Each measure
  records the host's load average at its start and end (Unix; Windows keeps none).

| Variable | Meaning | Unset |
|---|---|---|
| `S33_AB_ONLY` | the measures to run, comma-separated: `request`, `ca`, `gzip` | all three |
| `S33_AB_N` | a fixed n per series, instead of the stability rule | the rule |
| `S33_AB_BUDGET_S` | seconds a measure (each gzip body its own) may sample before it stops, reported as still moving | no budget |
| `S33_AB_LOAD` | `cpus` or a number of busy threads | none |
| `S33_AB_SIZES` | the gzip bodies' sizes in bytes, comma-separated | the sweep below |
| `S33_AB_BODY` | a file to send as the gzip measure's body, such as a real captured Messages request (sizes then unused) | synthetic bodies |
| `S33_AB_GZIP_LEVEL` | deflate level 0-9 | 6, zlib's default |
| `S33_AB_OUT` | a directory for the raw samples and `summary.txt` | stdout only |

## What it reports

A header gives the host, its OS and version, the revision (marked where the tree has uncommitted
changes), the CPU count and the load. Each series gets n, p50, p90, p99 and max, in µs for times
and bytes for sizes, plus the p99's distribution-free 95% interval and why sampling stopped.
`S33_AB_OUT` keeps every raw sample, one per line, as `<measure>.<series>.ns` or `.bytes`.

1. **`request`: the injector against direct egress.** The guest's request is a Messages-shaped
   POST that carries the credential's placeholder in `x-api-key`, to a local HTTPS host whose
   certificate chains to a test root. The arms:
   - `*.inject`: the run grants the host. The guest trusts the run's CA, and the injector ends its
     TLS, fills the value in and makes its own TLS to the host.
   - `*.direct`: the run has no grant. The same bytes go to the host as they came, and the guest
     trusts the host's root.

   The series:
   - `connect.*`: a new connection each sample. The sample runs from before the TCP connect to
     the whole response, with `Connection: close` so the host closes first and the client keeps
     no TIME_WAIT.
   - `request.*`: one request on a connection kept alive.

   The two arms alternate in every pair (AB, then BA). The host checks that it had the value
   (inject) or the placeholder (direct), and that the body arrived whole. Each arm's first
   exchange in the process, and each kept connection's first (which asks the lease), are left
   to `ca`.
2. **`ca`: the per-VM CA's cost.** Each sample is three fresh processes (the test binary run
   again as `inject::ab::child`). Each one starts as a VM's network process starts, and their
   order rotates every round.
   - `leaf.*`: the CA's key (`key`), the CA (`ca`), and a leaf minted first and second.
   - `inject.*`: key, CA, the upstream verifier (`upstream`, made at the process's start as
     `Prepared` makes it), then the process's first connection through the injector and its
     second. The child sends one request on each connection and stops when the whole response
     is in.
   - `direct.*`: a run with no grant, which makes no key: its first and second direct
     connections.

   `inject.first − direct.first` is the injector's share of a VM's first connection. The
   injector mints a leaf for every connection and keeps none (inject.rs `Ended::new`), so
   `inject.second` is also the steady cost of every later connection.
3. **`gzip`: the gzip-off effect (f70).** Claude Code skips compressing request bodies when
   `NODE_EXTRA_CA_CERTS` is set ("skips compression when you send requests through a proxy,
   configure a client certificate, or set `NODE_EXTRA_CA_CERTS`", its env-vars docs, line 316).
   The run's CA sets that variable (D242, step 2). Both arms run through the injector, each on
   its own kept connection:
   - `gzip`: the body compressed at the deflate level, sent `Content-Encoding: gzip`. The
     sample includes the compression, as the client pays it each request.
   - `identity`: the same body sent as it is.

   The host inflates a gzip body (as an API server would) and reports its length, which must
   equal the body's. `*.wire` is the TLS bytes the injector's upstream wrote to the host's
   socket for each request.
   - The default bodies are Messages-shaped JSON whose text is shards-net's own sources: the
     text a coding agent sends. Each repeat is further apart than deflate's 32 KiB window (RFC
     1951), so it compresses as the text itself does.
   - The sizes double from 2^14 (one TLS record's plaintext, RFC 8446 §5.1) to 2^26, the largest
     power of two within the Messages API's 128 MB request limit (platform.claude.com/docs/en/api/overview,
     "Request size limits", read 2026-11-13).
   - The largest sizes take minutes. Narrow them with `S33_AB_SIZES` or `S33_AB_BUDGET_S`.

## How n is chosen

There is no fixed count. Sampling starts at the fewest samples a p99 has a distribution-free
95% interval for (368: Le Boudec, *Performance Evaluation of Computer and Communication
Systems*, 2010, Theorem 2.1). It doubles n until every series' p99 falls within the interval its
previous round gave ("stable after R rounds"). `S33_AB_N` fixes n instead ("at S33_AB_N"),
and `S33_AB_BUDGET_S` ends a measure still moving ("its p99s still moving"). Quantiles are
nearest-rank.

## What it does not count

- The lease's round trip to the run host and kiln's broker. The harness answers a lease at once
  and holds it for the run.
- The audit line's write to the run host. It is formatted, then written to a sink.
- tcp.rs's segments and the guest's virtio path. The connection is carried as tcp.rs carries it
  (its `TO_HOST` and `TO_GUEST` windows, Nagle off), but by this loop, not the network process's.
  When nothing moves, the loop blocks on the host's socket.
- The guest is rustls with its defaults, not Bun's BoringSSL. It resumes TLS sessions where its
  server lets it: the direct arm's host sends tickets (rustls's default), and the injector, by
  design, sends none (inject.rs, R05).
- Claude Code's own compression level and size threshold are not in the evidence
  (docs/research/s33-veil-creds/evidence/claude-code). Level 6 is zlib's default
  (`Z_DEFAULT_COMPRESSION`, zlib.h). What Claude Code passes is unverified. Give a captured body
  and level when they are known.
