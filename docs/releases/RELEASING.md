# Releasing shards

A release is a tag `v<version>` on `dev`, made once `dev`'s CI is green.
`.github/workflows/release.yml` then builds `shards` for the 8 targets CI tests, attaches
each binary and `SHA256SUMS` to a GitHub Release, and that release gets its notes from
`docs/releases/v<version>.md`.

## What release.yml checks

- **The tag is the version** (job `guard`): the first `version = "..."` line of
  `crates/shards/Cargo.toml`, with a `v` before it, must equal the tag. No other crate's
  version is read: the workspace's other crates stay at 0.1.0.
- **The lock file agrees** (every build job): each target builds with
  `cargo build --locked --release -p shards` (musl's in Alpine through `scripts/musl-ci`),
  so `Cargo.lock`'s `name = "shards"` entry must carry the same version, or the build
  stops before compiling.
- **musl's `shards` is static**: no `INTERP` program header and no `NEEDED` entry
  (`readelf`).
- The release itself is made with `gh release create --generate-notes`, titled with the
  tag. The notes file replaces that afterwards (step 6).

## Steps

Names: `gits` is `/Users/adalundhe/.kiln-run/bin/gits`, git with kiln's commit signing.
Never turn signing off and never force-push. In zsh, a refspec like `$NEW:refs/heads/dev`
is mangled by the `:r` modifier: push through `/bin/bash -c '... "${NEW}:refs/heads/dev"'`.

1. **The version.** In `crates/shards/Cargo.toml` and in `Cargo.lock`'s `shards` entry.
   Check that the lock still holds with `cargo metadata --locked --format-version 1` (it
   resolves without building) and `cargo check --locked -p shards`.
2. **The notes.** `docs/releases/v<version>.md`: what the release brings since the last
   tag, for its users, every number taken from a PM entry
   (`docs/research/platform-measurements.md`). Links are absolute
   (`https://github.com/hyper-light/shards/blob/v<version>/...`), because the same text
   becomes the GitHub Release's body, where relative links break. Before tagging,
   `grep -n PENDING docs/releases/v<version>.md` must find nothing. Bring the README's
   platform lines and its "Where things stand" table, and D13's table in
   `docs/design/architecture.md`, up to what the code and tests show.
3. **Land on `gpu` by fast-forward.** The release branch must contain `origin/gpu`
   (`git merge-base --is-ancestor origin/gpu <branch>`); then
   `git push origin <branch>:refs/heads/gpu`. Wait for its CI (`ci.yml`) to pass on all 8
   targets.
4. **Land on `dev` as one signed commit with a single parent**, the tree of the `gpu`
   commit whose CI passed:

   ```sh
   git fetch origin
   G=$(git rev-parse origin/gpu)
   NEW=$(/Users/adalundhe/.kiln-run/bin/gits commit-tree -S "$G^{tree}" -p origin/dev -F msg.txt)
   test "$(git rev-parse "$NEW^{tree}")" = "$(git rev-parse "$G^{tree}")"
   /bin/bash -c 'git push origin "'"$NEW"':refs/heads/dev"'
   ```

   `msg.txt` is `Merge gpu <short sha>: ...`, what the landing brings and the CI run that
   validated it, ending with the attribution line. It must not say `[skip ci]`: dev's CI is
   the release's gate. Never merge with two parents. Do not check `dev` out in a worktree:
   moving it from another leaves that worktree's index stale.
5. **Wait for dev's CI** to finish green on every job (watch it to the end: only arm64
   macOS is tested locally; strip colours and `grep -a` the job logs for failures).
6. **Tag and release.** On the `dev` commit whose CI passed:

   ```sh
   /Users/adalundhe/.kiln-run/bin/gits tag -s v<version> -m "shards <version>" <dev sha>
   git push origin v<version>
   ```

   (v0.7.0 was an annotated tag, `git tag -a`.) The push starts `release.yml`. Once its
   `GitHub Release` job has passed, set the title and the notes:

   ```sh
   gh release edit v<version> --repo hyper-light/shards --title "shards <version>" \
     --notes-file docs/releases/v<version>.md
   ```

   Check the release lists all 8 binaries (`shards-darwin-arm64`, `shards-darwin-amd64`,
   `shards-linux-amd64`, `shards-linux-arm64`, `shards-linux-amd64-musl`,
   `shards-linux-arm64-musl`, `shards-windows-amd64.exe`, `shards-windows-arm64.exe`) and
   `SHA256SUMS`.
7. **Tell T4**, kiln's agent that runs its harnesses in shards microVMs through the CLI and
   the Engine API (kiln agent `01a15e86...`; find it with kiln's `agents_list`), with
   kiln's `agents_send`: the tag, the `dev` commit, the CI run that passed, and what in the
   release changes what T4 uses. If T4 is paused, the message waits in its queue.

Kernel releases (`kernel-*` tags, `kernel.yml`) are separate: a release names the kernel
its `crates/shards/src/kernel.rs` pins.
