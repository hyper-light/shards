# A member's root, environment, working directory and home: evidence record

This record follows [docs/development-evidence.md](../../../development-evidence.md) for step 7
of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d. It covers the four items the first step 7
lane left open (its record is [s7-scratch-volume](../s7-scratch-volume/README.md)). Each item
lands on its own, and its section is added as it lands. **Owed** marks what is not done, and
nothing owed counts as passed.

## Identity

- Task: plan step 7 (a member's HOME, env, image ENV and working directory), items 1-4 below.
- Branch: `p/s7-part2-v1`, from origin/main ad600dd52, each item merged with main as it lands.
- Policy: development-rules.md sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
- Role: implementer for Shards ROOT (01a148884a4770fdaf0111f3f792a248). Unit and light tests only.
- Owned paths: harness_env, the image's ENV, HOME, the working directory and mount order in
  `crates/init/src/domains.rs` and `staged.rs`, with their host twins; `skeleton()` was changed
  after step 8's owner agreed (its message of 2026-11-12).
- Targets: the seven of resources/supported-targets.txt. Init's part is Linux guest code.
- Hosts: adacachy (Linux 7.2.9-2-cachyos x86_64, glibc, rustc 1.98.1 from rust-toolchain.toml),
  reached by ssh when kiln's server did not answer.

## Item 4: a volume's parents keep the image's owner and mode

### Live example (R09)

`repro.py` replays init's steps at ad600dd52 (`skeleton`, `image_root`, `scratch`, staged.rs
`files`, then `child`'s mounts) with the same system calls, in a user and mount namespace
whose subordinate IDs let the image's directory be another user's:
`unshare --map-auto --map-root-user --mount python3 -I repro.py DIR`.

`repro-adacachy.log`, case `parents`: the image's `/home/node` is 1000:1000 0750, a volume is
mounted at `/home/node/.cache`, and the member sees `/home/node` as 0:0 0755. The skeleton
made every component of the mount point root's, 0755, and overlayfs reports a merged
directory's metadata from its upper layer alone (Documentation/filesystems/overlayfs.rst,
"Directories"; the leftmost lower layer is the topmost, "Multiple lower layers"). `/etc`, which
the skeleton always makes, and the root were shadowed the same way.

### Decision (R02, R03, R05, R12)

Recorded in D59 ("A member's root shows the image's directories as the image made them").
- Each directory the skeleton makes that the image has takes the image's owner and mode:
  fchownat, then fchmodat, as a chown clears set-ID bits (chown(2)).
- The skeleton tmpfs's root takes the image root's mode, uid and gid, as tmpfs options.
- A directory the image lacks stays root's, 0755, as before.
- No new literal. The 07777 mask is the permission and set-ID bits (inode(7)).

Not chosen: making no parent the image has. overlayfs needs every component of a missing
mount point in the skeleton layer, and a component the image has must be there too, for the
missing one beneath it.

### Red, green and mutation (R10)

`mutate4.sh` runs the new test, `domains::tests::a_skeletons_directories_are_the_images_own`,
against each variant of domains.rs that `mutants4.py` writes (`python3 -I mutants4.py BASE
mut4`, from the checkout's root), in a clone the carried files are laid over (`apply.sh`). The test runs the real `skeleton` and `image_root` in a
user and mount namespace and compares each directory with the image's. `mutate4-adacachy.log`:
- red, the skeleton at ad600dd52: fails (`/` 755 for 751, `/home/node` 755 for 750, `/etc`
  755 for 711);
- the owner and mode copy removed: fails (`/home/node`);
- the root's options removed: fails (`/`);
- green: passes, and passes again run as root of subordinate IDs, where owners are compared
  too.

`green4-adacachy.log` is the change merged with main 34215b23e (step 8's plug roots in): the
domains tests, the same as root of subordinate IDs (`/home/node 1000:1000 750 as 1000:1000
750`), rustfmt, clippy on shards-init with all targets, and all of shards-init's tests (210
passed, 1 ignored as before).

The test prints SKIP and returns where the host gives an unprivileged process no user and
mount namespace.

### State and code (R01, R04, R06, R07, R13)

- No panic path. The stat helper returns an Option, each system call's failure is an error, and
  no index or arithmetic is used.
- One `make` closure replaces two copies of mkdirat, so every skeleton directory follows one rule.
- Clippy on shards-init, all targets, `-D warnings`: clean on x86_64-unknown-linux-gnu (adacachy).

### Owed (ROOT)

- A real microVM on KVM, Hypervisor.framework and WHP: a member's view of a volume's parents
  (`stat` of `/home/node` under a volume at `/home/node/.cache`).
- With plug roots (D235), the root's own writable layer is the uppermost for `/`. Step 8's
  50ed65bbb gives it the image root's owner and mode; its record holds that evidence.

## Item 1: the image's ENV reaches each member

### Gap

At ad600dd52 nothing carried an image's config `Env` to init: the daemon composed it into the
run's own command's environment alone (run.rs `compose`), and init gave each member its own
`PATH`, `HOME=/tmp`, `SHARDS_DOMAIN`, its `run.env` and what D231 stages. The new unit tests
are the red: none of them can pass without the entries, the order and the refusals.

### Decision (R02, R03, R05, R12)

Recorded in D59 ("A member's environment: init's, the image's, its own, its run's"), with its
sources: OCI image-spec v1.1.0 config.md (`Env`), moby docker-v29.0.0 daemon/commit.go
`merge` and `CreateDaemonEnvironment`, Kubernetes' "Define Environment Variables for a
Container", and D231's own rule. The order, the last given winning: init's, the image's
(`PATH` folded under the member's own directories, `HOME` never the image's), `run.env`,
what is staged. `SHARDS_DOMAIN` is refused in the image's `Env` at build, by the daemon and
the run host, and by init, with one check (`shards_abi::run::members::variable_ok`) and one
text (`ImageEnvRefused`). No new literal: `image-env=` is a setup key beside D231's.

Alternatives weighed:
- the image's `HOME` as any other name: one value can be private to no more than one member,
  and it names the read-only system;
- the image's `Env` over `run.env`: the member's config is particular to it and written for
  it, and the image's is every member's and the command's.

### State and code (R01, R04, R06, R07, R13)

- No panic path: each entry is checked where it is read, refusals are `Err`, and no index or
  arithmetic is used.
- The refusal is typed (`shards_agentfile::agentfile::ImageEnvRefused`, the abi's `Error`),
  and its text is rendered once.
- `harness_env` takes the image's entries; the program lookup moved to `start`, after the
  staged variables, so the program is found on the `PATH` it runs with (`executable`).

### Red, green and mutation (R10)

`mutate1.sh` lays each variant `mutants1.py` writes over the change and runs the test that
must fail for it. `mutate1-adacachy.log`: the image's `Env` given no member, the image's
`HOME` taken as any other name, the program found on a `PATH` other than the one it runs with,
the build's check removed, and the daemon's entries never made each fail their test; the
change whole passes them all.

`tests1-adacachy.log`, the change on 1e6bbeada (main 34215b23e with item 4) on Linux x86_64:
shards-abi's members tests (10), all of shards-init (211 passed, 1 ignored as before),
shards-agentfile's agentfile tests (37), and shards' `agentfile::` and `api_run::` tests (18).
rustfmt and clippy (all targets, `-D warnings`) are clean on the four crates.

### Owed (ROOT)

- A member's `env` in a real microVM on KVM, Hypervisor.framework and WHP, of an image whose
  `ENV` sets `PATH`, `HOME` and another name, with a `run.env` and a staged variable over them.

## Item 2: a working directory the run names

### Change

`MemberSpec::workdir` (shards_api), the project path a member works in, reaches init end to
end: the image channel's request carries it, appended in version 5 (step 8's plugs took 4);
the run host stages it as `member-workdir=<index>\0<path>`; init enters it as the member
starts, where it entered `<dir>/<run.workdir>` before. It is absolute and plain, with no
empty, `.` or `..` component and within PATH_MAX (`shards_abi::run::members::workdir_ok`).
A relative or escaping one is refused by `shards_api::image` before the run host starts
(`ImageRefusal::Spec`, "a member's working directory"), by the run host (`api_run::staged`),
and by init, which reads the entries again (`staged::workdir`), as is a second one for one
member (`Error::Twice`). Init also holds the image's own `run.workdir` to the build's rule,
relative and never leaving its directory (`domains::inside`, the twin of shards_image osi.rs
`inside`), where before a crafted `osi.json` could name `../../etc`.

### Decision (R02, R05, R13)

Recorded in D231 ("A member's working directory"). The working directory must be there as
the member starts, as an exec's must be (D27, runc's exec), and none is made: a missing one
fails the start, `entering its working directory`. A process's working directory is the
directory it entered, under whatever is mounted there later, so a project plugged into a
running member (D235) is not the working directory of a member that started before it. Making
the directory where it is missing was weighed and not taken: beneath a plug root it would be
an empty directory that a later plug covers, and the member would work in that, never in its
project.

### Red, green and mutation (R10)

The new tests are the red: none can pass without the field, the entries and the checks.
`mutate2.sh` lays each variant `mutants2.py` writes over the change and runs the test that
must fail for it. `mutate2-adacachy.log`: a relative working directory taken as one, a second
staged by the run host, the client's check removed, init ignoring the entries, the image's own
`run.workdir` let out of its directory, and the request's working directories not read back
each fail their test; the change whole passes them all.

`tests2-adacachy.log`, the change on 6243ca378 (main 41e5a0943 with item 1) on Linux x86_64:
shards-abi's members tests (11), shards-api's image tests (15), all of shards-init (215
passed, 1 ignored as before), shards' `api_run::` tests (10), and the E2E test binary
`api_image` built. rustfmt and clippy (all targets, `-D warnings`) are clean on the four
crates, and clippy of shards-abi, shards-api and shards-init on aarch64 glibc and both musl
targets.

### Owed

- A boot-time way to have the project at its path as the member starts (a share given a
  member at its host path at boot, or a member started once its project is plugged): D235's
  and the orchestrator's to choose. Until then an orchestrator that plugs the project as the
  member runs names no working directory, and passes the project path to the harness itself:
  ACP's `session/new` takes a `cwd`, which "MUST be an absolute path" (agentclientprotocol.com,
  "Session Setup").
- Real-VM runs on KVM, Hypervisor.framework and WHP (ROOT): a member started in a named
  working directory, and refused one that is missing.

## Item 3: staged files land under the member's real HOME

### Live example (R09)

`repro-adacachy.log`, case `home`, replays ad600dd52: with `HOME=/home/claude` (a harness's
`run.env`) on a volume given the member there, `.claude/.credentials.json` staged for it
landed at `/tmp/.claude/.credentials.json`, its scratch, and `$HOME/.claude/.credentials.json`
did not exist in its view. The volume held nothing of it.

### Change

`start` now makes the member's environment first, then finds where its `HOME` lies among what
it may write as it starts (`home_in`): its scratch at `/tmp`, or a writable volume given it, the
deepest whose mount point (as declared, or as it lies in the image) is `HOME` or holds it.
`staged::files` writes beneath that mount, through init's own descriptor of it (a volume's
opened only where files are staged), making `HOME`'s
missing components 0700 and the member's. A file an earlier run left (a volume outlives its
run) is replaced: unlinked, never followed, then made with `O_EXCL | O_NOFOLLOW`; nothing of the
member runs yet to read it in between. A path staged twice in one run is refused before any is
written. Files staged for a member whose `HOME` lies in neither are refused, saying where `HOME`
is. `HOME` written with `..` is refused; empty and `.` components are dropped.

### Decision (R02, R05, R13)

Recorded in D231's staging bullet. Weighed and not taken: writing from the member's own view
in its first process, after its mounts. That is exact about symlinks in the image's path to
`HOME`, but a failure there is an errno and a step name, not the path and why, and the first
process may not allocate. Init's view has the same mounts by their mount points, and refuses
what it cannot place.

### Red, green and mutation (R10)

Unit tests: where `HOME` lies (`a_members_home_lies_in_the_deepest_mount_that_holds_it`); files
beneath a home made where missing (`a_home_beneath_its_mount_is_made_and_holds_its_files`);
replaced and refused twice (`a_file_from_before_is_replaced_and_one_staged_twice_refused`);
links never followed (`links_in_the_way_are_not_followed`); a volume's home opened by its path
only where files are staged (`a_volume_home_is_opened_where_files_are_staged`); no home refused
(`files_need_a_home`). `mutate3-adacachy.log`: the shallowest mount taken for the deepest, a
`HOME` with `..` placed, a path staged twice written twice, an earlier file left in the way,
and a link on the way followed each fail their test. `tests3-adacachy.log`: all of shards-init
on main aeac12d67 with the change (219 passed, 1 ignored as before); rustfmt and clippy clean
on x86_64 and aarch64, glibc and musl.

### Owed (ROOT)

- A real-VM run on KVM, Hypervisor.framework and WHP: a member whose `HOME` is a volume given it,
  a staged file read at `$HOME/...`, and a second run on that volume replacing it.

## Portability (R11, R12)

- Init's parts (items 3 and 4, and init's halves of 1 and 2) are Linux guest code: `domains`
  and `staged` compile for Linux alone. Their tests ran on Linux x86_64 (adacachy), and their
  clippy on x86_64 and aarch64, glibc and musl.
- The host parts (items 1 and 2: shards-abi, shards-agentfile, shards-api and shards) compile for
  every target. Their clippy is clean per crate on all seven targets
  (`lint-mac-pkgs-item1.log`, `lint-mac-pkgs-item2.log`, adacachy for x86_64 Linux glibc).
  Their light tests ran on Linux x86_64 (adacachy) and macOS arm64 (the Mac Studio,
  `lint-mac-item1.log`, `test-mac-item2.log`).
- The full `scripts/lint` stops at x86_64-pc-windows-msvc on main's own shards-vmm lib test
  (windows_process.rs:544, PM M526's SAFETY comment placement), outside this change. Its owner
  is fixing it, and the per-crate lint covers the targets after it.
- **Owed**: the host crates' light tests on Windows (jupiter had no idle cores; see
  Operations), and every real-VM run (ROOT).
- No platform adapter is added: the change rides existing seams (setup entries, the image
  channel, `skeleton`, `staged`).

## The rules, item by item (R01-R16)

- R01: no panic path in production code; the tests alone unwrap.
- R02, R03: each decision is in D59 or D231, with sources and the alternatives weighed. No new
  literal: `image-env=` and `member-workdir=` are setup keys, the bounds are PATH_MAX and
  NAME_MAX, and `07777` is the mode bits.
- R04: refusals are typed (`ImageEnvRefused`, the abi's `Error::Workdir` and `Error::Twice`,
  `ImageRefusal::Spec`). No new state machine: the start sequence's steps are as before, and
  the order is explicit in `start`.
- R05: no wait or cache is added. The volume's directory is opened only where files are staged.
- R06, R07, R13: small functions (`home_in`, `executable`, `inside`, `directory`). Comments
  carry sources and invariants.
- R08, R14: no performance claim is made. The added cost is one stat per skeleton directory and
  one open per staged home, unmeasured, and ROOT's to profile.
- R09: items 3 and 4 were reproduced live first (`repro.py`). Items 1 and 2 are gaps the code
  shows, so their red is their new tests.
- R10: red, green and guard-removal mutation for each item (`mutate*-adacachy.log`).
- R11, R12: see Portability.
- R15: implementer, unit and light tests only; real-VM runs are owed to ROOT. Each item landed
  on main by the person's 2026-11-12 rule, after ROOT's go.
- R16: the scratch on adacachy (`~/g121-tmp/s7p2`), the Mac Studio (`~/g121-tmp/s7p2`) and
  jupiter (`~/s7p2-tools`) is removed at the end; nothing else was touched.
