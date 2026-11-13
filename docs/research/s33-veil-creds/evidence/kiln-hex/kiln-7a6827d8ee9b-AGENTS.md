# kiln, for coding agents

kiln is one Rust binary: a CI/CD server, a deployer (Argo CD's model), a fleet of runners, and
the front end for coding agents (Claude Code, Codex, ACP agents, Antigravity, Devin), with its
web UI embedded. Read this before you change it, then the documents it points to.

## The rules

`docs/ci.md` §0 holds the rules for every line written here; §0.3 binds. In short:

- **No panics.** The workspace denies `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`,
  `unreachable!`, unchecked indexing and `exit` (`Cargo.toml`). Bad input becomes an error value
  shown to the person, never a crash.
- **hyper-rt, not tokio.** Async code runs on hyper-raft's `hyper-rt`; HTTP is kiln's own
  (`crates/kiln-http`, `docs/http.md`). Don't add tokio, hyper, h2, axum, reqwest, kube-rs or
  tokio-rustls. If hyper-rt lacks something, say so and ask.
- **Deterministic cores.** Logic that decides lives in `crates/kiln-ci` and `crates/kiln-cd` as
  pure code: no clock, disk, network or randomness except what's passed in.
- **SQLite is CI's system of record**, changed only by numbered migrations
  (`crates/kiln/src/store/migrations`).
- **The protocol only grows.** `crates/kiln/src/proto.rs` is postcard by variant index: new
  variants go after every existing one, and new capabilities are new bits.
- **General hardware.** Pick machines by what they can do, never by name.
- **The UI** (`ui/`): the hyperlight-site style; one primary surface per page; no popups
  (modals, sheets, floating panels); no effects (`useEffect` is a lint error; data comes through
  Suspense); static by default; a press never waits on the server to show it was taken (W21-R15:
  `ui/src/lib/action.ts`); checked in Chromium and WebKit at desktop and phone widths.
- **Opt-in automation.** What kiln decides on its own is off until declared, and says why it did
  what it did.
- **Seams.** kiln reaches other products only through `crates/kiln-seams` (`docs/seams.md`):
  each seam is a trait, a simulator and a conformance suite.
- **Change only what the task asks for.** Mention any other improvement instead of making it.

## IDs

- `Wnn-Rmm` and `Wnn-ACmm` are `docs/ci.md`'s requirements and acceptance criteria.
- `CP-T3.2`, `AC-T1.4.2`, `T-T2.1.1` and the like come from kiln's critical-path program. Its
  master plan is a kiln plan, not a file: `01a157fddb8276a2996246ea931c4808`. Read it with the
  kiln MCP tool `plans_get`; its §7 lists each track's plan (T1 Amphora to T10 Mantle).
- Cite the ID your change serves in code comments, commits and test names
  (`fn w05_r07_cancel_in_progress_cancels_running()`).
- Track branches are `cp/tN-*`. Don't edit another track's work: ask its agent (`agents_send`).

## Hazards

- **Never restart, swap, upgrade or reinstall a runner** (`kiln install`, `kiln join`, replacing
  `~/.local/bin/kiln`, its LaunchAgent or its service). Agents run through runners, likely you
  among them, and a restart ends their sessions. Say what the upgrade is; the person does it.
- **Production runs on adacachy, beside other work.** Heavy builds and tests go where kiln puts
  them, never to a machine by name: ask for what they need (`kiln run --needs`, or MCP `run` with
  `needs`). Your change's gate (below) holds each of its runs to what it asked and yields to
  production. Anything else heavy: `nice -n 10` and your own `CARGO_TARGET_DIR`.
- **Big files go under `~/.cache`, never `/tmp`.** On the Linux machines `/tmp` is a RAM disk
  every session shares.
- **Stop only what you started,** by the PID you recorded as you started it. Never `pkill -f` or
  `pgrep -f`: other sessions run the same commands.
- **No tree-wide `cargo fmt`.** kiln's long lines aren't rustfmt's; clippy is the check. For UI
  files, run `npx prettier --print-width 200 --write` on the files you changed, and only those.
- **Wire formats change append-only:** postcard variants, capability bits, new JSON fields
  optional.
- **Never print or commit a secret,** kiln's admin token included.
- **The person's word** is needed for every merge to main, every push and every deploy, and to
  create a repository. Deploys go through KilnCD, in merge trains (`docs/trains`).
- **Git:** never a bare `git stash`; rebase and squash only onto a fixed SHA; every push waits on
  the exit codes of the tests relevant to its change.

## Your change's gate

You compose it, not kiln. `ci/gate.sh` runs one step alone (`clippy`, or `test`; both when neither
is named), natively where it runs, on the crates you name, held to what its run asked for.

- **Scope:** `--since origin/main` takes the crates your change touches (committed or not), those
  whose code reads a file it changed by path (a test reading a doc or `.kiln/`: string literals,
  not comments) or builds a changed crate by path, and every crate depending on them. A changed
  file in no crate that none reads puts the whole workspace in scope.
  `ci/gate.sh --since origin/main --dry-run` says which and why; `-p <crate>` adds one.
- **Platforms:** clippy on Linux, macOS and Windows, each natively; tests on Linux, and on macOS or
  Windows too where your change has code for them.
- **Side by side:** `ci/gates.sh` starts a run per platform at once, each through `kiln run --needs`
  with the CPUs (`<N>`) and memory (`<M>`) its steps need, waits for each and keeps its output:
  ```sh
  ci/gates.sh --needs linux/amd64,cpus=<N>,mem=<M> --since origin/main \
              --needs macos,cpus=<N>,mem=<M> clippy --since origin/main \
              --needs windows,cpus=<N>,mem=<M> clippy --since origin/main
  ```
  Never detach a gate's run or leave it to a bare `wait`: either exits 0 whatever the run said.
- **Sized** by measurement, never a picked number: a step runs a rustc per CPU asked, is capped at
  the memory asked, and runs nothing without both. Each step says its peak memory (on Linux its
  anonymous memory, page cache left out; on macOS its processes' resident memory; on Windows
  none): ask that next time. Measured so far: kiln's own crate takes 7 GiB and more to build, and
  the libraries' tests peaked at 16.7 GiB at `-j 4` (`.kiln/ci.yaml`).
- **One verdict:** each run ends `GATE: PASS`; `GATE: FAIL` and why; or `GATE: YIELDED` (exit 75:
  it stopped for a strained machine; run it later or elsewhere). `ci/gates.sh` passes only when
  every run exited 0 and said `GATE: PASS`.

## Checks before handing over

The tests relevant to a change must pass before it's pushed. Full CI/CD runs (the gate, the whole
workspace's tests, the pipelines started by hand) wait for the person's explicit word.

| You changed | Run |
|---|---|
| Rust | what you touched first (its tests, clippy on its crate), then, when the person asks for the full run, your change's gate (above) |
| The UI | in `ui/`: `npm run typecheck`, `npm run lint`, `npm test` and `npm run build` |
| A seam or `docs/seams.md` | `cargo test -p kiln-seams` (its `doc_truth` test holds the doc to the code) |
| Vendored code | `python3 scripts/vendor.py check` |
| This file | `cargo test -p kiln --test agents_md` |

## Where things are

| Path | What |
|---|---|
| `crates/kiln` | the binary: server, runner, supervisor, installer, CLI and MCP server |
| `crates/kiln/src/harness` | coding agents as structured sessions (`docs/agents.md`) |
| `crates/kiln-ci` | CI's deterministic core: expressions, plans, scheduling (`docs/ci.md`) |
| `crates/kiln-cd` | the deployments' engine (`docs/cd.md`) |
| `crates/kiln-seams` | the seams to other products, with simulators and conformance suites |
| `crates/kiln-http` | HTTP/1.1 and HTTP/2 on hyper-rt (`docs/http.md`) |
| `crates/kiln-search` | the search across a change: kiln-seams' `Search` on vorpal (`vendor/vorpal`) |
| `ui/` | the web UI (TanStack Start), embedded in the binary |
| `docs/` | how each part works: start with `docs/architecture.md` |
| `vendor/` | vendored projects (`vendor/MANIFEST.toml`, `docs/vendoring.md`) |
| `.kiln/` | kiln's own CI and deployments |
