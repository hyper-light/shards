# Concrete closed process-identity producer map — proposal only

Root selected the closed producer-cause direction after [PROPOSAL.md](PROPOSAL.md),
while requiring public Refusal Clone/PartialEq/Eq to stay. No types below are
implemented or approved for production edits yet. No product execution, new
native run, global I/O rewrite, framework, dependency or cache was introduced.
Exact6db source/binary and [r222 failure evidence](README.md) remain unchanged.

POLICY_READ renewed for this specific lookup: full canonical main AGENTS,
R01–R16, evidence, CLAUDE, full managed plan and current updated handoff;
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`, implementer.
Root owns research selection, audits, profiling, heavy campaigns and final gates.
Only Held::find's actual Linux/macOS producers and specific callers are inspected.

## Exact proposed public values

Keep the existing Refusal variants and their fields/traits. Add only the reviewed
public envelope and these two domain enums (names remain reviewable):

```rust
Refusal::Process {
    pid: u32,
    expected_started: u64,
    failure: ProcessFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOperation {
    PidfdOpen,
    KqueueCreate,
    RegisterExit,
    OpenStat,
    ReadStat,
    ProcPidRusage,
    PollExit,
    KeventExit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessFailure {
    Os { operation: ProcessOperation, code: i32, kind: std::io::ErrorKind },
    PidOutOfRange { operation: ProcessOperation },
    DescriptorOutOfRange { returned: i64 },
    IdentifierOutOfRange,
    PathBounds,
    CapacityUnavailable,
    BufferBounds,
    InvalidStartRecord,
    DifferentStart { observed: u64 },
    Exited,
    UnrepresentedIo { operation: ProcessOperation, kind: std::io::ErrorKind },
}
```

Each operation names an actual existing call. The stage is explicit in that
operation: PidfdOpen/KqueueCreate/RegisterExit open the held process; OpenStat,
ReadStat and ProcPidRusage read start; PollExit/KeventExit check exit. Non-OS
variants name their exact producer/stage. The input PID and expected start are
retained in Refusal::Process; DifferentStart additionally retains the observed
start. No authority or terminal outcome is inferred from an errno or kind.

Os means a known native errno representation from one of the listed producers;
both its exact code and decoded kind are retained, including a zero code if that
is what the failed native operation reported. An OS ENOMEM remains Os at that
operation, rather than being relabelled CapacityUnavailable. The latter means
only the actual Vec::try_reserve failure; capacity overflow was already collapsed
into OutOfMemory by this reader, so it is not proof of exhausted physical RAM.

UnrepresentedIo is needed only at an io::Error representation boundary: raw OS
code is an Option, and File::open/read's public Result does not promise that
every future error has one. A no-code result at those fixed calls is explicitly
unrepresented, retaining operation/kind but **not claiming to retain a custom or
static payload**. It is uncertainty, not gone, a retry instruction or an authority
decision. There is no guessed errno, message parsing, String copy, custom-payload
clone or claim that a generic kind/errno projection represents arbitrary errors.
Native last_os_error currently guarantees a code; the same explicit fallback can
handle a violated representation expectation without unwrap/boxing.

## Original construction/source → direct proposed value

All line references are to `crates/api/src/taken.rs` at exact6db. Values are to be
constructed at the identified producer, not by interpreting its rendered text.
Existing interruption behavior and held-handle ordering remain unchanged.

| Producer | Original error construction/source | Direct typed result proposed |
| --- | --- | --- |
| Common find:34 | os::Held::open error formatted into String | Propagate its already-typed failure; client take_back supplies the same input PID/start in Process. |
| Common find:35 | os::started error formatted into String | Propagate the typed start producer value. |
| Common find:36–39 | Actual start mismatch, formatted expected/observed values | DifferentStart { observed: now }; expected start remains in public envelope. |
| Common find:41 | os.exited(false) error formatted into String | Propagate its typed PollExit/KeventExit cause. |
| Common find:42 | Existing exited=true result formatted into String | Exited. This retains existing observation semantics, not a new certification of the native event implementation. |
| Linux open:169 | pid_t::try_from(pid) failure boxed by io::Error::other | PidOutOfRange { operation: PidfdOpen }, before constructing any io::Error. The envelope retains the offending u32. |
| Linux open:172 | c_int::try_from(syscall_return) failure boxed by io::Error::other | DescriptorOutOfRange { returned }; retain the actual c_long return in i64 on supported Linux64-bit ABIs, rather than guessing errno or a descriptor. |
| Linux open:173–174 | Negative pidfd_open descriptor → io::Error::last_os_error | Os { operation: PidfdOpen, code, kind }; no PID/end inference from errno. |
| Linux stat_path:95–114 | checked_sub/get/u8 digit conversion/checked_add failures → ErrorKind::InvalidData conversion | PathBounds. The pathname remains the same derived stack path; no input limit, buffer guess or formatting added. |
| Linux started:82, File::open | Fixed generated path; std Unix File::open reaches native open64/open via cvt_r. Known native error uses last_os_error. | Os { operation: OpenStat, code, kind }; only a no-code representation uses UnrepresentedIo { operation: OpenStat, kind }. |
| Linux read_stat:123–124 | Vec::try_reserve(1) failure → io::Error::from(OutOfMemory) | CapacityUnavailable directly from that reserve result, before I/O/error rendering. |
| Linux read_stat:129 | Missing checked spare slice → InvalidData | BufferBounds. |
| Linux read_stat:129–131, File::read | Fixed std File read → FileDesc::read → libc::read/cvt; native error via last_os_error | Interrupted keeps the existing same-buffer retry; other native error becomes Os { operation: ReadStat, code, kind }. No-code representation becomes UnrepresentedIo { operation: ReadStat, kind }, never inferred to be native EINTR. |
| Linux read_stat:134–137 | used+count overflow or count outside initialized buffer → InvalidData | BufferBounds. |
| Linux start_field:149–160 | Missing last delimiter/field, UTF8 failure on decimal field, or u64 parse failure → InvalidData | InvalidStartRecord. This preserves the existing coarse InvalidData information; no new parser/reason framework. |
| Linux exited:193–195 | poll negative result → last_os_error; Interrupted retried | Os { operation: PollExit, code, kind } on terminal native error; same EINTR handling. |
| macOS started:222 | u32 PID→c_int failure boxed by io::Error::other | PidOutOfRange { operation: ProcPidRusage }, before error boxing. |
| macOS started:227–229 | proc_pid_rusage nonzero return → last_os_error | Os { operation: ProcPidRusage, code, kind }; preserve current native error source, no inferred process end. |
| macOS open:241 | u32→usize ident failure boxed by io::Error::other | IdentifierOutOfRange. It is unreachable on currently supported64-bit macOS from u32, but remains a checked typed refusal, not a silent cast or panic. |
| macOS open:243–245 | kqueue negative result → last_os_error | Os { operation: KqueueCreate, code, kind }. |
| macOS open:259–270 | kevent registration negative result → last_os_error | Os { operation: RegisterExit, code, kind }; existing owned kqueue drops on failure. |
| macOS exited:293–295 | kevent negative result → last_os_error; Interrupted retried | Os { operation: KeventExit, code, kind } on terminal native error; same EINTR handling. |

No allocation/error constructor exists in successful u8 initialization within
already-reserved Vec capacity, truncate, or the successful numeric field parse.
Those existing algorithmic invariants and resource costs are not expanded into
an allocation-free claim. Native objects continue to be captured/held before
start observation; no kill, lifetime, manager or API ownership change is proposed.
DescriptorOutOfRange explicitly means the return could not be represented; no
descriptor cleanup/success is claimed for that invariant-failure observation.

## Why the fixed File boundaries are closed, and where uncertainty remains

Pinned Rust is1.98.1, commit `48a229ceaefd4985c50990b14116b6d856af0985`:

- [Unix filesystem File](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/fs/unix.rs): File::open_c applies access/creation options, then cvt_r around open64; File::read forwards to FileDesc. Our call is File::open, whose read-only options are fixed and valid. Arbitrary OpenOptions errors are not claimed covered by this model.
- [Small C-string helper](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/helpers/small_c_string.rs): short paths use stack conversion; an interior NUL yields a static InvalidInput error. The actual generated path contains only fixed path bytes and decimal u32 digits and fits this pinned stack path. This is a source observation about std, not a runtime tuning constant adopted by Shards. A future unexpected no-code error remains UnrepresentedIo.
- [Unix FileDesc read](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/fd/unix.rs): the scalar read used here calls libc::read through cvt. No arbitrary Read implementation or custom reader is involved.
- [Unix cvt/cvt_r](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/pal/unix/mod.rs): native -1 becomes last_os_error, with cvt_r retrying interruption. [Core error](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/core/src/io/error.rs) distinguishes OS/simple/static/custom representations; raw_os_error does not expose a raw code for the others.

Thus native Os is a faithful representation of an actual OS error at a named
operation, while allocation/path/buffer/parser/conversion causes come directly
from the concrete producer branches. The model does not purport to clone or
reproduce an arbitrary io::Error's custom payload. Unknown representations are
explicitly unrepresented and never upgraded into terminal/authorization facts.

## Exact implementation scope needed after review

Minimum candidate paths remain `taken.rs` for this internal process-identity
producer route; `lib.rs` for public types/Refusal Display; `client.rs` for
take_back's typed wrapping; and focused failure/attach assertions plus conformance
matching documentation. Scope/ownership approval and meaningful light proof
precede production edits. The concrete signature proposal is:

- `taken::Held::find(pid,start) -> Result<Held, ProcessFailure>`.
- Existing private OS open/started/exited and Linux path/read/field functions
  return ProcessFailure directly from the table, without first constructing
  io::Error::other. The fixed File calls still produce std I/O errors at their
  own two explicit observation boundaries.
- `taken::started(pid) -> Result<u64, ProcessFailure>` and
  `taken::Held::exited() -> Result<(), ProcessFailure>` change their **private
  crate** signatures too. Actual non-find callers are ShardsGuest::identity and
  the take_back watcher respectively. Their existing formatting expressions can
  still use ProcessFailure's Display, but remain allocating/unproven reporters;
  this is not a claimed fix of either reporting path.

This avoids inventing a reverse io::Error bridge that boxes or silently discards
the new conversion/path/capacity distinctions. Public Guests/Guest signatures
continue to return Refusal. ProcessFailure Display must write its named operation,
code/kind and numeric fields to the supplied formatter, without constructing an
owned String or reconstructing a dynamic std I/O error for propagation. Private
signature and diagnostic changes, including effects on those separate reporters,
require root's explicit scope review before implementation. No control/lifetime
logic of those callers is proposed to change.

ShardsGuest::identity's separate allocating reporter, watcher reporting, later
attach Arc/thread/path/Box success allocations and conformance formatting remain
outside this reproduction's proof. Do not silently claim they are fixed. New
domain values must not be formatted back into Gone.why to propagate them.

The public Process variant requires downstream exhaustive Refusal matches to add
an arm; existing native attach Gone matches must migrate to precise typed
DifferentStart/Exited/observation outcomes. Old Gone and Clone/PartialEq/Eq stay;
no custom lossy clone/equality implementation or trait removal. OS/unknown errors
must not satisfy a conformance assertion for proven gone. Report formatting also
changes for native Process failures and must be documented as behavior change.
Root owns final compatibility/research selection and error audit. No new passing
test, producer error-path reproduction, cleanup proof or platform result is
claimed by this lookup; all actual results remain in the immutable r222 record.
