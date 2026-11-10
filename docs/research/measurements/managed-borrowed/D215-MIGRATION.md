# D215 borrowed connector migration note for coordinator composition

This is owned evidence only; root composes the architecture text. It describes
production53535ab6/current eaedd source, not adoption or a stable wire-ABI change.

| Existing caller/contract | Migration consequence |
| --- | --- |
| New `connect_borrowed(&Identity, port, patience)` | Returns existing boxed Stream with a separate typed BorrowError. It validates held process identity before/after the shared handshake, without acquiring Guest/lifecycle ownership, watcher, signal/stop, export or VM-copy behavior. Dropping its stream does not stop the process; the original lifetime owner is still required. |
| Owned `Guest::connect` zero/MAX | Preserve D215's zero one-dial/guest-answer mode and MAX no-clock mode. Do not replace those with the borrowed contract. |
| Borrowed zero/MAX | Zero is Expired before I/O. Unrepresentable absolute deadline is DeadlineOverflow before dial, including current-target Duration::MAX. No retry/unbounded borrowed mode. |
| Borrowed finite timeout | One original absolute deadline across process validation/connect/write/reply. Top-level Expired can occur around validation/boxing; handshake expiry retains Channel/HandshakePhase. Synchronous OS/scheduling delay is not a hard real-time promise. |
| Matching new errors | Match BorrowError, ProcessError, HandshakeError and their phase/cause fields. Inspect io::Error kind/raw OS code for I/O causes; do not parse presentation strings. Process I/O allocation failure can retain that process phase rather than becoming top-level Allocation. |
| Exhaustive matching of existing Refusal | Add `Refusal::Allocation { what }`. This public enum addition is source-breaking for downstream exhaustive matches. It truthfully identifies reachable connection storage/refusal allocation failure, not Boot or Unsupported. Its static reason requires no allocation to report. |
| Existing Refusal variants | Earlier variants and outer Display forms remain. Free-text handshake why now renders typed phase/IO kind/raw code, avoiding potentially allocating OS error-string formatting. Invalid path/timeout errors keep their kind with generic nonallocating error representation. These diagnostic text/payload changes are explicit. |
| Stream clone/settings/half-close | Existing public interfaces and shared timeout/nonblocking behavior remain. Clone can return nonallocating ErrorKind::OutOfMemory. The private one-state fallible Arc/fixed-Socket box implementation does not add a manager/cache/public allocation framework. |

Caller authority remains separate: the existing trusted Identity/socket and port
must already be authorized at the integration boundary. A PID/start/liveness check
is not muxer peer authentication or a new grant, and the borrowed function has no
GuestSpec allowlist/lifetime object from which to infer additional policy.

Current workspace Refusal consumers inventoried in the initial candidate record
are API lib/client/sim/conformance and API sim tests; shards boot tests use the API
without exhaustive Refusal matching. Current scoped builds/clippy cover those API
consumers. External/vendored callers (including Hex consumers) have not all been
built and must update exhaustive arms and borrowed deadline/error handling before
adoption. Do not allocate a fallback String/boxed error while handling OOM.

No installed binary, protocol tag, CONNECT/OK wire grammar, cryptographic key,
verification policy, VM image format or owned lifecycle privilege changes here.
Ordinary source compatibility still requires review because Rust public enums are
not implicitly extensible and diagnostic payloads changed as documented above.

Validation and limits: [CANDIDATE](CANDIDATE.md), [Mac](MAC.md),
[Windows](WINDOWS.md), [Linux](LINUX.md). The full Mac owned-Guest OOM abort is
not cured by the new variant: root traced a cold std OnceBox pthread-mutex
allocation before the fallible Socket path. No warmed-state remedy or
whole-library panic-free claim is adopted. The historical GNU timing assertion
red remains in the unchanged-source record; its causal test replacement and
actual marker-removal red/restored results are in [HELD-OUTPUT](HELD-OUTPUT.md).
The subsequent [non-UTF8 comm case](PROC-COMM.md) is now reproduced: legal raw
comm bytes incorrectly caused a typed Start/InvalidData before dial. Its isolated
byte-reader candidate preserves numeric/held-process/deadline semantics and
public types, but remains unadopted because a distinct actual std read32-byte OOM
abort blocks R01. No lifecycle/Mutex remedy or whole-library passing gate follows.
