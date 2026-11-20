# Windows checkouts and the bound on a path's length

A consumer of shards on Windows (the Hex lead, 2026-11-19) could not take it by git: cargo
refused its checkout, a path past MAX_PATH. `scripts/lint` now refuses a tracked path past
144 characters, the most every Windows checkout below holds without `core.longpaths`, and
the measurement folders that passed it were shortened (their `renamed.tsv`).

## The limits

- **Windows.** A path takes MAX_PATH, 260 characters with its terminating NUL, and a
  directory being made 12 fewer, unless the program is long-path aware and
  `LongPathsEnabled` is set ([Maximum Path Length Limitation](https://learn.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation)).
  Neither is by default: jupiter's `LongPathsEnabled` is 0.
- **cargo** (1.99.0, 5f94df478) checks a git dependency out with libgit2 1.9.6
  (git2 0.21.0, libgit2-sys 0.18.7+1.9.6), which refuses a checkout path past MAX_PATH UTF-8
  characters unless `core.longpaths` is set (`src/util/fs_path.c` `validate_length`, `<=
  MAX_PATH`; `src/libgit2/checkout.c` `checkout_target_fullpath`). Its file I/O takes `\\?\`
  paths (`src/util/win32/path_w32.c`), so no shorter limit applies to its directories.
- **Git for Windows** (2.56.0.windows.2, `compat/mingw.c`): `handle_long_path` writes a file
  whose full path is shorter than 260 with the NUL (the working directory's length, from
  `GetCurrentDirectoryW(0, NULL)`, counts it): 259 characters; `mingw_mkdir` a directory
  shorter than 248: 247.

## The prefixes consumers put before a path

| Consumer | Prefix | Characters |
| --- | --- | --- |
| cargo's checkout (`src/sources/git/source.rs`) | `C:\Users\<user>\.cargo\git\checkouts\shards-<16 hex>\<id>\` | 9 + user + 29 + 16 + 1 + id + 1 |
| `git clone` in the home | `C:\Users\<user>\shards\` | 17 + user |
| GitHub's runner | `D:\a\shards\shards\` | 19 |
| kiln's CI runner (kiln dc19ac9e5 `runner/runtime.rs` `Layout::new`; jupiter's `<work>`) | `C:\Users\<user>\AppData\Local\kiln\agent\work\w\<8 hex>\shards\` | 58 + user |
| kiln's synced copies (`reach/keep.rs`), written by kiln, read by git | `C:\Users\<user>\.kiln\reach\<runner>\<tree>-<10 hex>\` | 95 on jupiter, user `corph` |

- A user name holds at most 20 characters ([USER_INFO_1](https://learn.microsoft.com/en-us/windows/win32/api/lmaccess/ns-lmaccess-user_info_1),
  Remarks), and `C:\Users\<user>` is named for it.
- cargo's `<id>` is the commit's 7 hex digits, lengthened while they name more than one
  object of its database (`src/sources/git/utils.rs` `to_short_id`), up to all 40. Seven
  is no bound: `short-ids.py` counts main's 6,118 commits at 03c6c252e1 against this
  clone's 161,784 objects, and 6,112 take 7, 5 take 8 and 1 takes 9.
- With a user name of 20: cargo's prefix is 116 with an id of 40, the longest; kiln's CI
  runner's 78, the longest a `git clone` has; kiln's synced copies 110.

## The bound

260 − 116 = **144**. Through Git for Windows a path of 144 reaches 222 characters from
kiln's CI runner, its directory (at most 142) 220, and 254 from kiln's synced copies, which
git only reads: each under its limit. A path is counted in UTF-16 code units, as Windows
counts it.

## Before: main, 03c6c252e1, on jupiter

Windows 10.0.26200, Git 2.56.0.windows.2 (MSYS2's), cargo 1.99.0, `core.longpaths` unset,
`LongPathsEnabled` 0. 955 of main's paths are longer than 144, the longest 209.

- `git clone` into a directory of 77 characters (kiln's CI runner's prefix for a user name
  of 20, 78 with its separator) exits 128: 143 files `Filename too long`, then `fatal:
  unable to checkout working tree`. They are exactly main's 143 paths longer than 181
  (259 − 78): both sorted lists' SHA-256 is `9d3eb0e625730766ffc62925ec77c460c8dbe9443e7cf9c5130bd10ab3c15c09`.
- `cargo fetch` of shards-abi by git at 03c6c252e1, under a CARGO_HOME of 21 characters
  (`C:\Users\corph\.cargo`'s length) exits 101 as the Hex lead's did: `path too long:
  'C:/Users/corph/g121ch/git/checkouts/shards-81d553f225e85e22/03c6c25/docs/research/measurements/managed-coordinator-forward/raw-7ee6/root-managed-forward-guards-7ee6-v2/reply-corrupted-after-validation/sock-addr-relay4-unconnected-fixed-original-peer.identity.json'`,
  263 characters, as long as the Hex lead's under `C:\Users\corph\.cargo`.

## The change

2,024 files of 12 measurement folders moved, each byte for byte: run directories dropped
the words their folder already says, and a case directory's guest program records are its
`guest.*`. Each folder's `renamed.tsv` maps the moved files to the paths their runs wrote,
which their manifests, checksums and logs keep naming; their READMEs and the documents
that cite them name the new paths. The longest path is now 144; the longest directory 131.

The guard (`scripts/lint`, `== paths`) passes on the change and refuses a 145-character
path, naming it and the bound; it passes 143 characters and one past U+FFFF (144 UTF-16
units) and refuses 144 and one (145); it fails, exit 128, where `git ls-files` fails, never
passing a list it did not read. Through the same count main has 955 paths past it.

## After

`jupiter.sh` takes a fresh clone of the branch and a `cargo fetch` at its commit, the
CARGO_HOME 69 characters long, so that cargo's prefix is 116 with its id of 7, as with a
user name of 20 and an id of 40; and 70, one past. Owed: its run on the pushed branch.
