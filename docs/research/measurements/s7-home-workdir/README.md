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

`green4-adacachy.log` is the change merged with main c2c4547c1: the domains tests, the same
as root of subordinate IDs (`/home/node 1000:1000 750 as 1000:1000 750`), rustfmt, clippy on
shards-init with all targets, and all of shards-init's tests (207 passed, 1 ignored as before).

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
- With plug roots (D235), the root's own writable layer is the uppermost for `/`. Its owner and
  mode are step 8's to match (its message of 2026-11-12).
