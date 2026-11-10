# Read-only next-batch constant inventory

Source: canonical `c921a0b12b06d495b354907162e991376edffd41`, inspected from
`p/managed-cancel-events`. No product change outside registry/http.rs. These are
source classifications and experiment proposals, **not reproduced defects or closed
R05 items**. Primary source byte hashes are in [sources.json](sources.json).
The coordinator assigns each future owned implementation and actual live reproduction.

| Value / owner | Classification and actual source | Proposed derivation / decisive next proof |
| --- | --- | --- |
| `http::DIAL_POLL=100 ms`, this lane | Unproven polling period; no protocol source. Actual held libc-worker red/green and independent notification mutations in this batch. | Removed: completion/cancel event plus real deadline. No replacement period. |
| `http::RACE=300 ms`, registry | Picked upstream default, serving a required address-attempt schedule. RFC8305 §5 recommends 250 ms, requires >= 10 ms between attempts, recommends 100 ms minimum / 2 s maximum, and describes SYN/RTT-derived delay. Its existing comment alone does not satisfy R03. | Research/measure per-path SYN/RTT estimator and uncertainty/cold-start inside researched RFC constraints, preserving early-failure and family semantics. Do not delete pacing merely because DIAL_POLL was a probe. |
| `vm_run::RECORD_FOR=1 s` KVM / `50 ms` HVF, capture-owned path | Measurement copied into product: comments cite PM M33 nested-host 125 ms and PM M30 9.9 ms versus 1.5 ms recording cost. These validate working-set recording/prefetch, not universal end durations. No ABI/protocol guarantee says application startup ends then. | Prefer the actual first request/answer/exit boundary where present; for long-running/no-answer work, compare observed marginal working-set benefit/cost within admitted owner budget. Need live actual-recording traces for long startup, cold/warm, HVF/KVM/WHP, load and short/long workloads before choosing an online stopping rule. No new fixed replacement copied from those samples. |
| `init::SELFTESTS_WAIT=2 s`, `await_crypto_selftests` 1 ms sleep, capture-owned path | Unproven timeout/probe. PM M21 demonstrates crypto work replay and roughly 20 ms extra boot waiting in 8 saves; it does not derive 2 s or 1 ms. Current code logs and saves on timeout or read error; it does not stop the kernel self-tests. | Preserve all crypto tests. Prove the kernel's actual completion authority/availability first; a proc file watch is not automatically a completion event. A trustworthy completion interface or kernel-backed readiness observation may replace probing; unsupported/error states must stay explicit. Real guest proof must hold self-tests in progress, observe actual completion, failure and no early-snapshot mutation. No blind timeout removal or `cryptomgr.notests`. |
| `build/proxy/server::CONNECTIONS=512`, quality-owned path | Unproven admission cap; comment states only that further connections wait in listener backlog. No source/derivation accompanies 512. Per-proxy number does not itself bound aggregate per-process FDs, threads, TLS state and body files. | Derive charged concurrency from real existing FD/task/memory authority minus protected control and measured per-flow cost, across all steps; use an online throughput window within that cap. Do not enlarge entitlement from learned throughput or blindly use OS backlog as a resource budget. Quality lane owns modifications/live proof. |
| `rego/funcs/regex::CACHE_MAX=100`, unassigned | Copied upstream default: OPA v1.14.1 regex.go `regexCacheMaxSize`. Shards cache is per thread while OPA's fallback map is global; 100 is not an aggregate cap. Shards template_match uses `cache(...,false)` and bypasses eviction just as pinned OPA `getRegexpTemplate` does. Source confirms unbounded template-cardinality retention, not a measured hostile resource failure. | First reproduce mixed ordinary/template cache semantics with pinned OPA oracle and memory growth/churn. Then decide whether compiled state belongs to an existing evaluation/policy owner and whether caching beats rebuilding. Charge bytes/eviction under that owner's actual budget if retained; test exact output/invalidation/cleanup. No picked replacement count or new global cache. |

## Crypto completion source check

[Linux v6.18 crypto/proc.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/crypto/proc.c)
is a seq-file view: `c_show` derives `selftest:passed/unknown` and `type:larval`
from algorithm flags; `crypto_init_proc` registers that sequence. This source does
not expose a poll/readiness callback or user completion FD. Therefore FileWatch
notification on that pathname is not evidence of algorithm completion.
[algapi.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/crypto/algapi.c)
`crypto_start_tests` at late_initcall schedules unstarted test larvals. An internal
kernel completion is not by itself a supported guest userspace notification API.
An actual kernel protocol/interface comparison and VM fixture remain owed.

## Regex cache compatibility hazard to reproduce first

[OPA v1.14.1 regex.go](https://raw.githubusercontent.com/open-policy-agent/opa/v1.14.1/v1/topdown/regex.go)
`getRegexp` and `getRegexpTemplate` share `regexpCache[pat]`, but compile differently;
`getRegexpTemplate` neither includes delimiter bytes in its key nor evicts. Shards
likewise shares only pattern text between these callers; ordinary regex is unanchored,
while a template with no substitutions quotes/anchors literal text.

This produces a **source-inferred observable compatibility hazard**, not yet a live
result: ordinary `regex.match("a.b","axb")` followed by
`regex.template_match("a.b","axb","<",">")` can reuse wildcard compilation;
reversing order can reuse quoted literal compilation. Changing eviction/removing
this cache can alter that mixed-call behavior. Cached entries are not automatically
semantics-free optimizations just because they are compiled programs. Confirm the
actual pinned oracle sequence and the required compatibility contract before a fix;
record any deliberately corrected upstream bug as an explicit deviation. No new
oracle/rego implementation was run or modified in this registry batch.
