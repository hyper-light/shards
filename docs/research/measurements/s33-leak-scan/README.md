# Step 33's leak scan: a run's whole memory, its disk and its members' environments, scanned for a credential's bytes

Plan step 33 (kiln plan 01a1d2b88be57304b972761e2a5c6781, step 10's first part; goal #70) says
no credential value is anywhere in a harness's microVM: not in its memory, not on its disk, not
in its environment ([D242](../../../design/architecture.md)). This directory holds the harness
that checks that claim. Its real runs belong to ROOT. This file names the exact commands and the
evidence each run keeps.

The harness has two parts:
- **The save** ([D259](../../../design/architecture.md)): `ImageGuest::save(to, patience)`.
  It saves an orchestrator's run whole and leaves it running: its memory, its vCPUs' and
  devices' state, its swap and its writable layer. It writes them as a snapshot directory at
  `to`. `to` belongs to the run's owner: it must be beneath the run's home and beneath no
  directory the guest reaches, and nothing may be there yet.
- **The scan**, [scan.py](scan.py). It reports where a credential's bytes lie in what the save
  wrote, in a disk image, and in each member's environment as given. It also lists every
  credential-shaped string that no value it was given explains. It never prints a value.

## Identity

| | |
|---|---|
| Policy | `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, AGENTS.md `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`, `docs/development-evidence.md` `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`, all read at origin/main `6d8b64262`. Role: implementer (goal #70's child for step 10, part A) |
| Research | kiln research 01a1d2b88be57304bf6b081fe46832e0: f18 (the bring-up's leak check) and f19 (ImageGuest had no save) |
| Light tests | adas-mac-studio-3.local (Adas-Mac-Studio-4.local): macOS, arm64, Python 3.9.6, rustc 1.98.1. They ran through a carrier: a bundle over origin/main in a blobless clone, `~/g121-tmp/s33-leakscan` |

## The method, and where its numbers come from

The scan reuses the bring-up's leak check where it lives (R12),
[harness-bringup-v2/credentials.py](../harness-bringup-v2/credentials.py):
- **What counts as a secret.** Every string a JSON document holds under a name containing
  `token`, `key`, `secret` or `account_id`, whatever its length.
- **What counts as a leak.** The whole value, or any run of `RUN = SHOWN + 1 = 9` of its
  consecutive bytes, looked up at every offset. `SHOWN` is 8 because OpenAI's refusal shows a
  key's first 8 characters (harness-bringup-v1's record, which `credentials_test.py` reads).

Research f18 describes the older form of that check: 24-byte windows looked up at every 8th
offset, which promised runs of 31 bytes. That form predates the review
([review.md](../harness-bringup-v2/review.md), F1), which found all three numbers chosen and
replaced them as above. A run of 31 bytes is still found, and so is any run of 9 to 30. A run of
`RUN - 1` bytes, what a provider shows of a key, is not reported.

scan.py adds four things for a saved memory, which credentials.py did not need for a directory
of logs:
1. **Each value in the forms memory holds it in.** Its UTF-8 bytes. Its UTF-16LE, as
   JavaScriptCore and Windows hold wide strings: `RUN` characters. Its base64 in both alphabets
   (RFC 4648 §4 and §5), from each of the three offsets its first byte can take in a group of
   three, whole groups only, as in whatever larger encoding it lies.
2. **Public values.** These are values the run holds by design, its placeholders
   (`--public`). A window of a secret that a public value also holds, in any form, is not looked
   for. So a placeholder that keeps the value's marker (`sk-ant-oat01-`) is not a leak. The
   value's own bytes still are.
3. **Credential shapes.** These are the shapes the bring-up scanned its records for: `sk-` keys
   (Anthropic's and OpenAI's), JWTs in compact form (RFC 7515 §7.1, `eyJ` being base64url of
   `{"`), and bearer credentials (RFC 6750 §2.1).
   - A shape's random part is at least 22 base64url characters. RFC 6749 §10.10 bounds the odds
     of guessing a token at 2^-128, and a character carries 6 bits, so it takes
     ceil(128 / 6) = 22 of them.
   - A shape must start a token, so `disk-` holding `sk-` is not one.
   - Each shape is classed as a secret's (already counted as a leak), a public value's, or
     unexplained. Unexplained ones are listed by where they lie.
   - Token field names (`access_token`, ...) are counted only: every harness's code holds them.
4. **Large files read in place, in linear time and a bounded heap** (#74 step 10). Each file
   is mapped (`mmap`), never copied. A run of a secret's bytes lies inside a run of its form's
   own bytes: `RUN` or more units, each a byte, or a UTF-16LE code unit. Only those runs are
   looked in.
   - **The walk.** A run is found by its first `RUN` units, then walked to its end a host page
     (`mmap.PAGESIZE`) at a time. The old scan matched each run whole, and sre keeps a frame
     for each pass of a repeated group, so its heap grew with a UTF-16LE run. A saved memory
     on WHP ran out of memory that way (MemoryError at scan.py:156, jupiter, 2026-11-13).
   - **Anchors.** Inside a run, the windows are looked up through anchors: `width // 2 + 1`
     bytes of a window, read every `stride = width + 1 - anchor` bytes. A window holds an anchor
     at `stride` consecutive offsets, so exactly one of them is read (pigeonhole). Where an
     anchor is held, each window that holds it there is looked up. Every window at every
     offset is still found. Any split of `width + 1` finds the same windows; halving it keeps
     an anchor no shorter than the stride it saves, and saves the most offsets.
   - **Shapes.** A credential shape is tried only where a token begins (a lookbehind). A run
     of one shape's prefixes is walked once, not once from each prefix: a JWT's was
     quadratic. One finding differs from the old scan's, deliberately: a shape that starts a
     token inside a match that doesn't (`x` + `eyJ0.` + a JWT) is now listed. The old scan
     skipped that whole match, and the JWT inside it with it.

**Exit codes.**

| Code | Meaning |
|---|---|
| 0 | Nothing found |
| 3 | A secret's bytes were found (credentials.py's code for a leak) |
| 4 | No secret's bytes were found, but some credential-shaped strings are unexplained. A person looks at each, by its path and offset |
| 2 | A document or file could not be read. Only its type and file are named |

**Output.** Each finding is one line: its path, offset and length, and the name its value is
held under (`given.json:env_token`). No line holds a value.

## Commands ROOT runs

Each command runs from a checkout at or after this harness's landing.

**1. The harness's own check, on real microVMs** (KVM x86_64, HVF arm64, WHP x86_64).

    cargo test --profile test-release -p shards --test api_image -- --exact a_saved_runs_memory_holds_what_its_harness_was_given_and_the_scan_finds_it

- **What it does.** It boots the test image through shards-api. Its harness echoes its stdin
  and is given two fresh values: one in its environment, one in a staged file.
- **The refusal.** It asks for a save outside the run's home and expects `Destination`, with
  nothing made.
- **The save.** It saves into the home and checks for a snapshot directory with a memory file.
  It echoes a line after, so the run is still going.
- **The scans.**
  - It scans the save and the harness's environment for the two values given. Expected: exit 3,
    with both found in the save's `memory` file by their names and the environment's value
    found in its file.
  - It scans again for a value never given. Expected: 0 secret runs found.
- Python 3 runs scan.py: `python3`, or `python` on Windows (jupiter has it).
- **Where to run it.**
  - HVF: adalundhe-macbook-pro or adas-mac-studio.local.
  - KVM x86_64: adacachy2.
  - WHP: jupiter.
  - KVM arm64: through L1, `SHARDS_L1_TESTS=api_image:a_saved_runs_memory_holds_what_its_harness_was_given_and_the_scan_finds_it`
    with boot.rs's `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`, run through
    `launchd-run.sh` (CLAUDE.md).
- **Evidence.** The test's output. Its home and diagnostics show if it fails (`common::TempDir`).

**2. Step 33's acceptance: real sessions** (after step 8 lands, the sign-in profiles, and Hex's
half of step 6 is merged by the Hex lead; step 3's injector and kiln's half of step 6 are
built). On each backend:
1. **Run the session.** The orchestrator (Hex, through shards-api) boots the signed-in Claude
   Code, or Codex, image with its credential grants. It runs a tool turn and forces a refresh,
   as plan step 10 says.
2. **Save it.** The orchestrator calls `guest.save(<SHARDS_HOME>/s33-saves/<run>, patience)`
   on that run. It needs a hook for this: through Hex, that hook is the Hex lead's to add. Keep
   the home on a tmpfs where the host has one, so that the save never reaches a disk.
3. **Write the members' environments.** Write each member's environment, as the orchestrator
   gave it, to `<private>/env/<member>`, one `KEY=VALUE` per NUL. Use a 0700 directory of the
   caller's alone.
4. **Scan:**

       python3 -I docs/research/measurements/s33-leak-scan/scan.py \
         --secrets ~/.claude/.credentials.json --secrets ~/.codex/auth.json \
         --secrets <private>/refreshed.json \
         --public <private>/placeholders.json \
         <SHARDS_HOME>/s33-saves/<run> <the image's disk> <private>/env

   - `refreshed.json` holds the tokens the refresh issued: kiln's broker holds them, outside
     the VM.
   - `placeholders.json` holds what the run was given in their place: each grant's
     `CredentialGrant::placeholder`, as the orchestrator minted it.
   - Expected: exit 0, or exit 4 with each listed shape explained by a person by where it lies.
     Exit 3 is a leak.
5. **Remove the save.** It holds everything the guest held:

       rm -rf <SHARDS_HOME>/s33-saves/<run>

**Evidence each run keeps** (in `records/<backend>-<harness>/`, committed):
- The scan's whole output. It has counts, paths, offsets, lengths and names, never a value.
- The save's directory listing and sizes (`ls -lR`).
- The orchestrator's run diagnostics (`ImageGuest::keep_diagnostics`), checked for values with
  scan.py before commit.
- **Not kept:** the save itself, the documents of secrets, and the placeholders.

## Light tests run here

All on adas-mac-studio-3.local, through the carrier:
- **The scan.** `python3 -I docs/research/measurements/s33-leak-scan/scan_test.py -v`: 7
  passed (9 since #74 step 10, below). They cover:
  - planted 1 MiB memory images: a value's 31-byte run found where it lies, whole; `RUN - 1`
    bytes, and none at all, found clean;
  - runs of `RUN` and of 30 bytes found at each of eight offsets;
  - a disk image, an environment file and a short secret found by path and name;
  - UTF-16LE and the six base64 forms found;
  - a placeholder with the value's marker clean, and the value itself found;
  - an unexplained JWT listed by where it lies (exit 4), explained when given as public; a
    literal prefix and a shape inside a longer word not counted;
  - an unreadable document giving exit 2;
  - no value in any output.
- **The bring-up's check, unchanged.**
  `python3 -I docs/research/measurements/harness-bringup-v2/credentials_test.py`: 15 passed,
  with credentials.py's `secrets` now built on `named_secrets`.
- **Guard removals** (R10, by hand, each test run on its own): 13 of 13 failed, the scan's seven
  among them. They are listed in D259.

## The linear scan (#74 step 10), measured

**The defect** (ROOT's E2E logs, R09):
- WHP, jupiter, Python 3.13.0: `api_image`'s
  `a_saved_runs_memory_holds_what_its_harness_was_given_and_the_scan_finds_it` ran 1185 s, then
  died with MemoryError at scan.py:156 (`for candidate in pattern.finditer(buf)`), in the
  UTF-16LE pass.
- KVM: the same test passed, in 2092 s.

**Reproduced** (red, committed alone): `scan_test.py`'s
`test_long_runs_of_every_forms_bytes_scan_in_bounded_memory_and_values_in_them_are_found`. Its
4 MiB image (`runs_image`) holds long runs of a secret's bytes in each form, with 19 values
planted inside them. On the old scan the heap peaked at 192616128 traced bytes. Also red on the
old scan: `test_a_shape_inside_a_word_hides_no_token_after_it`.

**Before and after** on `runs_image`, one scan per process:

    python3 -I measure.py image PATH SIZE
    python3 -I measure.py scan SCAN_PY PATH time|heap
    python3 -I measure.py prefixes SCAN_PY BYTES

The old scan.py was origin/main 9d2da923d's; the new one is this commit's. Both ran on each
host through the carrier.

| Host | Image | Old: time, heap peak, resident peak | New: time, heap peak, resident peak |
|---|---|---|---|
| adalundhe-macbook-pro, macOS arm64, Python 3.14.6, 16 KiB pages | 16 MiB | 1.68 s, 587127800 B, 584695808 B | 0.86 s, 710268 B, 47284224 B |
| | 64 MiB | 7.75 s, 2239349400 B, 2245115904 B | 3.51 s, 710268 B, 97845248 B |
| adacachy2, Linux x86_64, Python 3.14.9, 4 KiB pages | 16 MiB | 1.43 s, 587127712 B, 577908736 B | 0.64 s, 281300 B, 40960000 B |
| | 64 MiB | 6.38 s, 2239349312 B, 2239635456 B | 2.50 s, 281300 B, 90951680 B |

- **Time** is from `time` runs; **heap peak** is tracemalloc's, from `heap` runs.
- **Resident peak** is getrusage's. It includes the image's mapped pages, which are clean and
  reclaimable.
- **The old heap** grew at 33 to 35 bytes per byte scanned. A memory of several GiB therefore
  runs out, as it did on WHP.
- **The new heap** is the same at 16 and 64 MiB: the index and one page's match.
- **The same findings.** Each pair of runs said the same lines, digest for digest: 19 of 19
  values found, at their offsets.

A run of JWT prefixes (`prefixes`, `eyJ` repeated):

| Bytes | Old, Mac | New, Mac | Old, Linux | New, Linux |
|---|---|---|---|---|
| 30000 | 0.171 s | 0.001 s | 0.176 s | 0.001 s |
| 60000 | 0.731 s | 0.001 s | 0.697 s | 0.001 s |
| 120000 | 3.161 s | 0.003 s | 2.823 s | 0.002 s |

The old time grew quadratically, about 4× for each doubling; the new time is linear.

**Old against new, findings.** 400 random images on this Mac (Python 3.9.6) gave the same leaks
and the same shapes, digest for digest: 1702 leak findings and 379 shape findings. The images
mixed zeros, slices of secrets and of public values in every form, runs of one byte and of one
UTF-16LE unit, shape prefixes, JWTs and random bytes. That harness was ad hoc and is not kept.
The one difference is the deliberate one under "Shapes" above.

**Light tests, this slice:** on adalundhe-macbook-pro and adacachy2, through a carrier (a
bundle over origin/main in a blobless sparse clone, removed after):
- `scan_test.py -v`: 9 passed;
- `credentials_test.py`: 15 passed;
- the old scan under the new tests: the two new tests failed, the other seven passed.

**Guard removals** (R10, each on a copy, all 9 tests run): 6 of 6 failed:
- the walk as one unbounded repeat;
- the walk stopping after its first page;
- a stride one past the pigeonhole;
- the verify range missing its stride's first offset;
- anchors from a window's first offset alone;
- shapes tried anywhere, not at a token start.

The walk stopping, and the stride, first survived a single plant in the middle of a run.
`runs_image` now plants a value across each of the walk's first `RUN` steps, each at its own
offset to its step.

## Holes not read (PM M561)

A save's `memory`, `swap` and `layer` are sparse files, and the run's shape sizes its hot-plug
room and swap window to the host's memory. On CI's x86_64 runners (16 GiB) the harness's own
check spent 28.6 minutes in the scan's two passes, reading holes byte for byte.
- **What it reads.** Only each file's data, as lseek(2)'s SEEK_DATA and SEEK_HOLE give it.
  Each range is widened into the holes beside it by the most zeros a finding can take: the
  zero bytes a window or a short secret starts or ends with (one, for a UTF-16LE window of
  ASCII). Holes are whole blocks, so two widened ranges never meet.
- **When it reads holes.** Where a window or short secret is all zeros, so a finding could
  lie in a hole, and where the system cannot say where holes are (Windows).
- **Its summary** says the bytes it read beside the files' length: `(N bytes, M read)`.
- **Tests:** `scan_test.py` 11 passed (two new), `credentials_test.py` 15 passed, on
  adas-macbook-pro-2 (Python 3.9.6). The new tests were red on the old scan. Guard removals:
  3 of 3 failed. A 512 MiB hole took one pass 153.8 s on the old scan and 0.68 s on the new.

## Owed

- **Run 1, the harness's own check,** on KVM x86_64, HVF arm64, WHP x86_64 and KVM arm64
  (L1). Not run here: no VM is booted in this lane. Since #74 step 10, a rerun on WHP (where
  the old scan ran out of memory) and on KVM (2092 s) shows the linear scan on a real save.
- **Run 2, the real sessions.** It waits on step 8, on Hex's half of step 6, and on a save hook
  in Hex.
- **Step 10's second part,** the injector's A/B latency harness, is another lane's.
