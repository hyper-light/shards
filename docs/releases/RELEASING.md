# Releasing shards

A release is a tag `v<version>` on a commit of `main` whose CI (`ci.yml`) passed on every
job. All work lands on `main`; `dev` is no longer the release branch (it is not an ancestor
of `main`). Pushing the tag starts `.github/workflows/release.yml`, which builds `shards`
for the seven targets of `resources/supported-targets.txt`, attaches each binary and
`SHA256SUMS` to a GitHub Release, and the release then gets its notes from
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
  tag. The notes file replaces that afterwards (step 5).

## Steps

Names: `gits` is `/Users/adalundhe/.kiln-run/bin/gits`, git with kiln's commit signing.
Never turn signing off and never force-push. In zsh, a revision followed by `:` (a refspec
such as `$REL:refs/heads/main`, or `$M:path`) is mangled by zsh's `:` modifiers: write
`"${REL}:refs/heads/main"`, or push through `/bin/bash -c`.

1. **The version.** In `crates/shards/Cargo.toml` and in `Cargo.lock`'s `shards` entry.
   Check that the lock still holds with `cargo metadata --locked --format-version 1` (it
   resolves without building).
2. **The notes.** `docs/releases/v<version>.md`: what the release brings since the last
   tag, for its users. v0.7.0's tag sits on a `dev` commit whose tree is `main`'s
   90de623f, so what is new since it is `90de623f..main`. Every number comes from a PM
   entry (`docs/research/platform-measurements.md`) or the D-entry that records it, and each
   item names its D or PM entry and the commits on `main` that made it. Links are absolute
   (`https://github.com/hyper-light/shards/blob/v<version>/...`), because the same text
   becomes the GitHub Release's body, where relative links break. Bring the README's
   platform lines, its command table and its "Where things stand" table up to what the code
   and tests show. Before tagging, `grep -n PENDING docs/releases/v<version>.md` must find
   nothing.
3. **Land on `main` by fast-forward**: merge `origin/main` into the release branch with
   `gits merge`, check the notes cover what came in, and push the branch's head to `main`
   (`git push origin "${REL}:refs/heads/main"`), which git refuses unless it fast-forwards.
4. **Wait for `ci.yml` on that commit** to finish green on every job (watch it to the end:
   only arm64 macOS is tested locally; strip colours and `grep -a` the job logs for
   failures). A later push to `main` cancels a run still waiting, so the commit tagged is
   one whose own run finished green.
5. **Tag and release**, on that commit:

   ```sh
   gits tag -s v<version> -m "shards <version>" "$REL"
   git push origin v<version>
   ```

   (v0.7.0 was an unsigned annotated tag, `git tag -a`; use that if the signing program
   refuses a tag.) The push starts `release.yml`. Once its `GitHub Release` job has passed,
   set the title and the notes:

   ```sh
   gh release edit v<version> --repo hyper-light/shards --title "shards <version>" \
     --notes-file docs/releases/v<version>.md
   ```

   Check the release lists all seven binaries (`shards-darwin-arm64`, `shards-linux-amd64`,
   `shards-linux-arm64`, `shards-linux-amd64-musl`, `shards-linux-arm64-musl`,
   `shards-windows-amd64.exe`, `shards-windows-arm64.exe`) and `SHA256SUMS`.
6. **Tell T4**, kiln's agent that runs its harnesses in shards microVMs through the CLI and
   the Engine API (kiln agent `01a15e86c8eb7397a5555a65dce1fc51`, named Isolation), with
   kiln's `agents_send` (or `kiln agents send --queue ID TEXT`): the tag, the commit, the CI
   run that passed, and what in the release changes what T4 uses. If T4 is paused, the
   message waits in its queue.

Kernel releases (`kernel-*` tags, `kernel.yml`) are separate: a release names the kernel
its `crates/shards/src/kernel.rs` pins.
