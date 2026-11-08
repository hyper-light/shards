# Building images: `shards build` as `docker build`, with RUN steps in microVMs

Research note, 2026-09-29. Evidence only; nothing here is a final design decision.

It informs `shards build`: a drop-in for `docker build` whose RUN steps execute in microVMs rather than containers, and whose images `shards run` can run. Phase 4 lists build in its scope [shards: docs/design/architecture.md:1084-1088]. Two notes already chose the broad architecture, and this one does not repeat them:

- container-engine-internals.md §2.6 and R9: reimplement BuildKit's model on shards' own runtime, with content-addressed steps, diffs taken from the overlay upper directory with explicit whiteouts, and `SOURCE_DATE_EPOCH` clamping.
- image-storage.md R5: builds execute in a builder microVM; the upper directory is the snapshot, streamed once into an OCI tar and an EROFS image; local layers stay uncompressed.

This note supplies what they lack: the exact semantics to reproduce, and how to execute them in microVMs. The goal is parity wherever a user or a script can see a difference:

- which Dockerfiles are accepted, and the error for the rest;
- the image: its config, its history and its layers;
- what the build prints, and its exit status;
- which steps are cached.

Pinned sources (citation paths are relative to each repository; abbreviations as listed):

| Tag | Source |
|---|---|
| `bk` | moby/buildkit v0.33.0, `dddd5621af04`, also tagged dockerfile/1.27.0. `df/` = `frontend/dockerfile/`, `d2l/` = `frontend/dockerfile/dockerfile2llb/`, `ins/` = `frontend/dockerfile/instructions/`, `ui/` = `frontend/dockerui/`, `ref.md` = `frontend/dockerfile/docs/reference.md`, `ops/` = `solver/llbsolver/ops/`, `pui/` = `util/progress/progressui/`, `V/` = `vendor/`, `ctrd-archive/` = `vendor/github.com/containerd/containerd/v2/pkg/archive/` (containerd v2.3.4) |
| `buildx` | docker/buildx v0.37.1, `0b265a9f62db`. `C/` = `commands/` |
| `cli` | docker/cli v29.8.1, `4a63305d7433`. `I/` = `cli/command/image/` |
| `moby` | moby/moby docker-v29.8.1, `464cd50c3d9e`. `bn/` = `daemon/internal/builder-next/` |
| `is` | opencontainers/image-spec v1.1.1, `147f9c13cedb` |
| `ddocs` | docker/docs at `e58e9f955d8a` (main, 2026-09-29). `B/` = `content/manuals/build/` |
| `ctr` | apple/container 1.5.0, `d265d669ecae` |
| `cz` | apple/containerization 0.47.0, `bc994b88df46` |
| `shim` | apple/container-builder-shim 0.13.1, `e18d2182fd06` |
| `kata` | kata-containers 4.2.0, `c7351e797eff`. `shimv2/` = `src/runtime/pkg/containerd-shim-v2/`, `vc/` = `src/runtime/virtcontainers/` |
| `ctrd-docs` | containerd v2.4.1 (`f2551031d727`), `docs/snapshotters/erofs.md` |
| `gmv` | go-microvm v0.0.41, `7e148d855378`, the local checkout `../go-microvm` |
| `linux` | Linux 6.18.48 from the stable tree at git.kernel.org, the version shards' kernel builds [shards: scripts/build-kernel.sh:12] |
| `go` | Go 1.26.1 standard library, the local toolchain (BuildKit's `go.mod` asks for 1.26.3) |
| `bk4279` | moby/buildkit PR #4279, "Add options to specify containerd runtime (alternative)", merged 2023-09-26 (`28a9fd552884`) |
| `kata13517` | kata-containers issue #13517, "agent: Preserve binfmt_misc registrations for the sandbox lifetime", opened 2026-07-30, open |
| `assets` | anonymous registry fetches of `ghcr.io/apple/container-builder-shim/builder:0.13.1` and `moby/buildkit:v0.26.2`, 2026-09-29 |
| `shards` | this repository at `a1b501d`; the lines cited are unchanged through `b71ab2b`, except the phased plan in `docs/design/architecture.md`, which moved 11 lines down. `PM Mn` = docs/research/platform-measurements.md |

Why these versions:

- BuildKit v0.33.0 is the newest release on 2026-09-29, from `git ls-remote`, and the Dockerfile frontend's newest tag, dockerfile/1.27.0, is the same commit.
- Docker Engine and buildx both run it: moby docker-v29.8.1 and buildx v0.37.1 require BuildKit v0.33.0 [moby: go.mod:67; buildx: go.mod:32].
- docs.docker.com's Dockerfile reference is BuildKit's `ref.md`, mounted at v0.33.0 [ddocs: hugo.yaml:356-363; go.mod:16].
- docker/cli v29.8.1 and moby docker-v29.8.1 are the versions the other notes pin. apple/container 1.5.0 and Containerization 0.47.0 are shipping-the-guest.md's; `container` 1.5.0 pins builder-shim 0.13.1 [ctr: Package.swift:25, 603].

Identity checks (with `cmp`):

- moby's vendored copies of BuildKit's `parser`, `instructions`, `dockerfile2llb`, `shell`, `dockerui`, `exporter/containerimage`, `progressui`, `solver/llbsolver/ops` and `cache/contenthash` are byte-identical to the checkout's. So are buildx's `progressui` and `dockerui`.
- moby/patternmatcher v0.6.1 is byte-identical in BuildKit's, the CLI's, moby's and buildx's vendor trees.
- docker/docs' vendored `reference.md` is byte-identical to `ref.md`.

Markers:

- **(derived)**: our arithmetic or comparison on cited facts.
- **(inference)**: reasoning from code or docs, not stated there.
- **(probed)**: the pinned code itself, run on the input shown by a scratch harness that imports it: BuildKit's `parser`, `shell` and `progressui` packages, the vendored patternmatcher, and Go 1.26.1's `fmt`, `encoding/json` and `archive/tar`. The harnesses are not committed; §3 item 2 proposes committing them as an oracle.
- **(measured)**: our own measurement, from shards' committed benchmarks.
- **UNVERIFIED**: no acceptable source found.

## 1. Scope

- **Q1.** The Dockerfile language as BuildKit parses it: lines, directives, comments, heredocs, exec and shell forms, case, errors.
- **Q2.** Every instruction's semantics, flags and variable expansion, including the environment a RUN sees, and how COPY and ADD resolve sources and keep metadata.
- **Q3.** Multi-stage builds and `--target`.
- **Q4.** The build context and `.dockerignore`.
- **Q5.** The image produced: config, history, layer diffs, reproducibility, image ID.
- **Q6.** Caching.
- **Q7.** The `docker build` command line and output a drop-in must match.
- **Q8.** Prior art for building in VMs rather than containers.
- **Q9.** Implications for shards (§3), and what needs our own measurement (§4).

## 2. Findings

### 2.1 At a glance: what BuildKit does with each instruction

| Instruction | LLB | Executes in | Output | Cache key covers | History |
|---|---|---|---|---|---|
| FROM | image source | the daemon (resolve, pull) | the base's layers | manifest digest and platform | the base's, inherited |
| RUN | ExecOp | one runc container per step | the root mount's diff | args, env, cwd, user, hostname, mounts, network, security, platform; read-only mounts' contents | `RUN … # buildkit`, layer |
| COPY, ADD (local) | FileOp copy | the daemon, through fsutil | the diff | the action, and a content hash of the selected sources without mtimes | `COPY … # buildkit`, layer |
| ADD (URL, git) | HTTP or git source, then FileOp | the daemon | the diff | content digest, `Last-Modified`, ETag; commit | `ADD … # buildkit`, layer |
| WORKDIR (not `/`) | FileOp mkdir | the daemon | the diff | the action, including its time | `WORKDIR /x`, layer |
| everything else | none | the frontend | config only | nothing of its own; enters later RUN keys through env, user and cwd | one entry, `empty_layer` |

Sources: §2.4–2.10, where each row's facts are cited.

- Only RUN runs a process from the image. Everything else is a file operation or a download in BuildKit's own process [bk: solver/llbsolver/file/backend.go:272-287; client/llb/fileop.go:566-629] (derived).

### 2.2 Q1: the Dockerfile language

The published reference is BuildKit's `ref.md` (see "Why these versions"). It and the code disagree in places (§2.14); where they do, the code decides what users see.

**Lines**

- **Line endings and the BOM.** CRLF works: `trimNewline` strips `\r\n` [bk: df/parser/parser.go:519-521, 551-563]. A UTF-8 BOM is stripped from the first line only [bk: df/parser/parser.go:294-299; df/parser/directives.go:198-200].
- **Length.** A physical line over 65,535 bytes fails: `dockerfile line greater than max allowed size of %d` [bk: df/parser/parser.go:565-572].
- **Continuation.** The escape token at the end of a line continues it, even before trailing spaces or tabs, unless the token is itself escaped [bk: df/parser/parser.go:153-170].
  - The regex cannot see an escaped escape before a continuation (`foo \\\`), and says so in a comment [bk: df/parser/parser.go:165-167].
  - Joining removes only the escape and the newline. Continuation lines keep their leading whitespace, and only the first line is left-trimmed [bk: df/parser/parser.go:320-347, 540-549]. So `RUN echo a \` then `    b` runs `echo a     b` (probed).
  - Comment lines inside a continuation are dropped [bk: df/parser/parser.go:336-339]: `echo c\`, `# c`, `d` gives `echo cd` (probed).
  - Blank lines inside a continuation are skipped with the warning `Empty continuation line found in: …` [bk: df/parser/parser.go:340-343, 351-358], which BuildKit reports as the `NoEmptyContinuation` lint [bk: d2l/convert.go:260-271].
- **Comments.** A line whose first non-blank character is `#`. A `#` anywhere else is an argument character [bk: df/parser/parser.go:523-526; ref.md:55-81].

**Parser directives: `syntax`, `escape`, `check`**

- **Form.** `# key=value` with a case-insensitive key matching `^([a-zA-Z][a-zA-Z0-9]*)\s*=\s*(.+?)\s*$`, each key at most once: `only one %s parser directive can be used` [bk: df/parser/directives.go:15-25, 42-44, 72-83].
- **Where.** Scanning stops at the first line that is not a comment (a blank line too), at a comment that is not `key=value`, or at an unknown key [bk: df/parser/directives.go:50-76].
  - `  # escape=\`` with leading whitespace, and `# ESCAPE = \``, are honoured; a comment or blank line before it disables it (probed).
  - A `#!` first line disables `escape`, but syntax detection skips it [bk: df/parser/directives.go:134-142, 190-196].
- **`escape`** is `` ` `` or `\`, else `invalid escape token '%s' does not match ` or \\` [bk: df/parser/parser.go:153-156].
- **`syntax`** also accepts `// syntax=` and a whole-file JSON `{"syntax": …}` [bk: df/parser/directives.go:117-188]. It, or the `BUILDKIT_SYNTAX` build arg, which wins, hands the whole build to that frontend image through `gateway.v0` [bk: df/builder/build.go:34, 54-69, 227-258]. A drop-in that runs no frontend images must refuse or ignore any `syntax` other than its own (inference).
- **`check`** takes `skip=<rules|all>`, `error=<bool>` and `experimental=<…>`, separated by `;`, else `invalid check option %q` [bk: df/linter/linter.go:187-240].

**Instructions**

- **Case.** Instruction names are lower-cased before dispatch [bk: df/parser/parser.go:194-221, 232; ins/parse.go:81]. `AS` is case-insensitive; stage names are lower-cased and must match `^[a-z][a-z0-9-_.]*$` [bk: ins/parse.go:420-434].
- **Unknown words** parse, then fail in the instruction pass: `dockerfile parse error on line %d: unknown instruction: %s`, plus ` (did you mean X?)` within Levenshtein distance 2 [bk: ins/parse.go:145, 160-177; util/suggest/error.go:9-65].
- **Flags** are case-sensitive and must be written `--name=value`; booleans also accept a bare `--x`. Errors: `unknown flag: --x`, `duplicate flag specified`, `missing a value on flag` [bk: ins/bflag.go:142-214].
- **Before the first FROM**, only ARG: anything else is `no build stage in current context` [bk: ins/parse.go:185-212; ins/commands.go:547-553]. An empty file is `the Dockerfile cannot be empty`, and no FROM at all `dockerfile contains no stages to build` [bk: d2l/convert.go:237-239, 277-279].

**Arguments: JSON (exec) form and shell form**

- **RUN, CMD, ENTRYPOINT, SHELL** (`parseMaybeJSON`): JSON only if the text starts with `[` and unmarshals to an array of strings [bk: df/parser/line_parsers.go:278-328].
  - An array holding a non-string is a hard error: `when using JSON array syntax, arrays must be comprised of strings only`.
  - Any other JSON failure silently falls back to shell form. `CMD ["echo","hi"] # c`, `ENTRYPOINT ['a','b']` and `RUN ["c:\x"]` all become shell-form strings wrapped in `/bin/sh -c` (probed).
- **COPY, ADD, VOLUME**: JSON, else split on whitespace, so paths with spaces need JSON [bk: df/parser/line_parsers.go:333-344; ref.md:1678-1679].
- **FROM and EXPOSE** split on whitespace; **USER, WORKDIR, STOPSIGNAL, MAINTAINER** take the rest of the line [bk: df/parser/line_parsers.go:244-275].
- **ENV and LABEL** are quote-aware, with the legacy `KEY rest-of-line` form when the first word has no `=`. `ENV A` alone is `ENV must have two arguments`; a later bare word is `Syntax error - can't find = in %q. Must be of the form: name=value` [bk: df/parser/line_parsers.go:140-174].
- **Shell form is wrapped** in `Config.Shell`, else `["/bin/sh", "-c"]` (Windows: `["cmd", "/S", "/C"]`), plus the words joined by one space [bk: ins/support.go:5-19; d2l/convert.go:1914-1922; d2l/defaultshell.go:3-8]. That applies to RUN, CMD and ENTRYPOINT [bk: d2l/convert.go:1407-1411, 1599-1606, 1615-1622]. A shell-form HEALTHCHECK is stored as `CMD-SHELL` and wrapped by dockerd at run time [bk: ins/parse.go:622-633; moby: daemon/health.go:346, 454-456].
- **SHELL** must be JSON: `SHELL requires the arguments to be in JSON form` [bk: ins/parse.go:784-804; ins/errors_unix.go:7-9].

**Heredocs**

- **Where.** RUN, COPY and ADD, or ONBUILD wrapping one, never in JSON form [bk: df/parser/parser.go:82-98, 128-140].
- **Recognition.** A word matching `^(\d*)<<(-?)\s*([^<]*)$`, lexed with `\` as escape whatever the directive [bk: df/parser/parser.go:118-123, 481-499; df/shell/lex.go:516-533]. `<<` must start a word: `RUN cat<<EOF` is no heredoc, and its body lines become unknown instructions (probed).
- **Bodies** are read raw, comments included, until a line equal to the name. `<<-` strips leading tabs; several heredocs per line are allowed; end of file is `unterminated heredoc` [bk: df/parser/parser.go:371-396, 501-504].
- **Expansion** happens when no part of the delimiter was quoted [bk: df/parser/parser.go:428-465].
- **RUN** [bk: d2l/convert.go:1354-1406]:
  - one heredoc that is the whole command and starts with `#!` (not Windows) is written 0755 to a read-only scratch mount at `/dev/pipes/<NAME>` and run as `<shell> /dev/pipes/<NAME>`;
  - one heredoc otherwise becomes the `-c` script. The frontend never expands it: "we ignore the expand option";
  - anything else (`<<EOF python3`, several heredocs) is rebuilt as `line\n<body><NAME>…` for the shell to interpret.
- **COPY and ADD** turn each body into a 0644 file named after the delimiter, copied with `--chmod` if given [bk: d2l/convert_copy.go:297-321]. A heredoc as the destination is `%s cannot accept a heredoc as %s` [bk: ins/parse.go:280-282, 818-820].

**Errors and warnings**

- Everything above called an error is fatal.
- The linter's rules only warn [bk: df/linter/ruleset.go:7-194]. Among them: `StageNameCasing`, `FromAsCasing`, `NoEmptyContinuation`, `ConsistentInstructionCasing`, `JSONArgsRecommended`, `MaintainerDeprecated`, `UndefinedVar`, `UndefinedArgInFrom`, `LegacyKeyValueFormat`, `MultipleInstructionsDisallowed`, `WorkdirRelativePath`, `SecretsUsedInArgOrEnv`.
- `# check=error=true` makes them fatal, `lint violation found for rules: …`, raised when the result is finalized [bk: df/linter/linter.go:123-139; d2l/convert.go:893-899].

### 2.3 Q2: variable substitution

**Where it runs, and against what**

- Each instruction with an `Expand` method is lexed at dispatch, with the escape directive's token, against the stage's environment [bk: d2l/convert.go:290, 1033-1063]:
  - the base image's `Env`, plus `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` when it has none (not on Windows) [bk: d2l/convert.go:792-808; util/system/path.go:13-25];
  - each ARG of the stage that has a value [bk: d2l/convert.go:1708-1711];
  - each ENV, in order, replacing an existing name [bk: d2l/convert.go:1340; client/llb/meta.go:49-61].
- One value applies throughout an instruction: `ENV a=1 b=$a` sees the old `a` [bk: ref.md:474-485].
- Unset variables expand to `""` and are recorded; the `UndefinedVar` lint reports them unless they are declared ARGs [bk: df/shell/lex.go:547-565; d2l/validations.go:190-209].

**Which instructions expand what**

| Instruction | Expanded | Source |
|---|---|---|
| FROM | the image name and `--platform`, against global ARGs only | [bk: d2l/convert.go:397-458] |
| ARG | the default value, only when no `--build-arg` gives one; never the name | [bk: d2l/convert.go:1039-1040, 1693-1704] |
| ENV, LABEL | keys and values | [bk: ins/commands.go:156-158, 194-199] |
| WORKDIR, USER, VOLUME items, STOPSIGNAL | the argument | [bk: ins/commands.go:335-342, 427-434, 444-446, 456-463] |
| EXPOSE | the words, then split again on whitespace | [bk: d2l/convert_expose.go:15-25] |
| COPY, ADD | sources, destination, `--chown`, `--chmod`; ADD's `--checksum`; heredoc bodies with an unquoted delimiter | [bk: ins/commands.go:215-243, 263-315] |
| RUN | only the values of `--mount` options | [bk: ins/commands.go:372-377; ins/commands_runmount.go:173-178] |
| not at all | RUN's command, CMD, ENTRYPOINT, SHELL, HEALTHCHECK, MAINTAINER, ONBUILD, `--exclude`, `--network`, `--security`; build-arg values | (derived: no `Expand`) [bk: d2l/convert.go:1693-1696, 2173-2174] |

- **`--from` refuses variables** outright. COPY: `variable expansion is not supported for --from, define a new stage with FROM using ARG from global scope as a workaround` [bk: d2l/convert.go:942-952]. RUN `--mount`: `'from' doesn't support variable expansion, define alias stage instead` [bk: ins/commands_runmount.go:179-182].
- **RUN's progress name**, not its command, is shown with variables expanded, secret values masked as `****` [bk: d2l/convert.go:1466-1475; d2l/convert_secrets.go:94-113].

**Forms** [bk: df/shell/lex.go:338-485]

- `$name`, `${name}`; names are letters, digits and `_`. `$1`, `$@` and `$$` are looked up as variables of those names, so they expand to empty; a `$` before anything else stays literal [bk: df/shell/lex.go:487-545].
- `${v:-w}`, `${v-w}`, `${v:+w}`, `${v+w}`, with the usual set-versus-empty rules [bk: df/shell/lex.go:401-410].
- `${v:?msg}` and `${v?msg}` fail the build. Their default messages are `NAME: is not allowed to be unset` and `NAME: is not allowed to be empty` [bk: df/shell/lex.go:411-426].
- `${v#p}`, `${v##p}`, `${v%p}`, `${v%%p}`, `${v/p/r}`, `${v//p/r}`. Patterns know only `*` and `?`, with `\` escapes, and no bracket expressions [bk: df/shell/lex.go:427-481, 595-646].
- `${}` is `syntax error: bad substitution`; any other modifier is `unsupported modifier (%s) in substitution` (probed).

**Quoting**

- Single quotes are literal. Inside double quotes, the escape token escapes only `"`, `$` and itself. Unquoted, it escapes any character and is dropped [bk: df/shell/lex.go:215-336]. So `a\nb` gives `anb`, `"a\nb"` gives `a\nb`, `\$FOO` gives `$FOO`; under `escape=\``, `\$FOO` gives `\` plus the value (probed).
- Quote removal happens here, not in the parser. So `WORKDIR "/a b"` is `/a b`, while shell-form `COPY "a b" /x` splits into `"a` and `b"` and fails on an unterminated quote (derived).
- **COPY heredoc bodies are not POSIX here-documents.** With an unquoted delimiter every `\` escapes the next character and is dropped (`printf "a\nb"` becomes `printf "anb"`), and quotes are not special; a quoted delimiter keeps the body raw [bk: ins/commands.go:230-243; df/dockerfile_heredoc_test.go:146-229] (probed).
- Variable names are case-sensitive, except in a frontend built for Windows [bk: df/shell/equal_env_unix.go:8-10; df/shell/equal_env_windows.go:8-17].

### 2.4 Q2: the instructions that only change the config

**What they share**

- **History.** Every entry comes from `commitToHistory`: `created_by` is the message, `comment` is `buildkit.dockerfile.v0`, `empty_layer` is the opposite of "made a layer", and `created` is the `SOURCE_DATE_EPOCH` time or nil, for the exporter to fill (§2.9) [bk: d2l/convert.go:44-47, 1816-1828].
  - ` # buildkit` is appended only for RUN, COPY and ADD, the instructions that pass a state [bk: d2l/convert.go:1507; d2l/convert_copy.go:354].
  - FROM and ONBUILD write no entry [bk: d2l/convert.go:713-740, 1590-1593].
- **Layers.** Only RUN, COPY, ADD, and WORKDIR other than `/`, make one. The entries this section's instructions write all have `empty_layer: true`, except WORKDIR's [bk: d2l/convert.go:1542-1564].
- **The step count** in progress names counts RUN, COPY, ADD and WORKDIR, plus one for a FROM that pulls [bk: d2l/convert.go:487-499] (§2.11).

**Per instruction** (config fields as image-spec names them [is: config.md:136-192]; OnBuild, Shell and Healthcheck are Docker's additions [bk: V/github.com/moby/docker-image-spec/specs-go/v1/image.go:11-54])

| Instruction | Config change | `created_by` | Source |
|---|---|---|---|
| ENV | `Env`: an existing name replaced in place, a new one appended | `ENV k=v …`, values after expansion and quote removal | [bk: d2l/convert.go:1331-1344, 1754-1768] |
| LABEL | `Labels`, merged over the base's | `LABEL k=v …` | [bk: d2l/convert.go:1574-1588] |
| ARG | none: the value enters RUN's environment only (§2.5) | `ARG k=v k2`, resolved values, build-arg values included | [bk: d2l/convert.go:1677-1729] |
| WORKDIR | `WorkingDir` | `WORKDIR <normalized path>`; a layer unless `/` | [bk: d2l/convert.go:1510-1565] |
| USER | `User`, verbatim | `USER <u>` | [bk: d2l/convert.go:1642-1649] |
| EXPOSE | `ExposedPorts` | `EXPOSE [80/tcp …]`, the raw words string-sorted first | [bk: ins/parse.go:680-696; d2l/convert_expose.go:15-44] |
| VOLUME | `Volumes`; no directory is made | `VOLUME [/a /b]` | [bk: d2l/convert.go:1651-1662] |
| CMD | `Cmd`; `ArgsEscaped: true` in every form | `CMD ["/bin/sh" "-c" "x"]`, Go's `%q` of a `[]string` | [bk: d2l/convert.go:1595-1609] |
| ENTRYPOINT | `Entrypoint`; clears `Cmd` unless this stage ran CMD | `ENTRYPOINT […]` likewise | [bk: d2l/convert.go:1611-1627] |
| SHELL | `Shell` | `SHELL [/bin/bash -c]` | [bk: d2l/convert.go:1672-1675] |
| STOPSIGNAL | `StopSignal`, as written | `STOPSIGNAL <s>` | [bk: d2l/convert.go:1664-1670] |
| HEALTHCHECK | `Healthcheck` | Go's `%+v` of the struct: `HEALTHCHECK {Test:[CMD-SHELL …] Interval:5m0s …}` | [bk: d2l/convert.go:1629-1640] |
| ONBUILD | `OnBuild`, the trigger text | none | [bk: d2l/convert.go:1590-1593] |
| MAINTAINER | top-level `author`, not a label | `MAINTAINER <name>` | [bk: d2l/convert.go:1569-1572] |

- The `%q` and `%+v` renderings were checked with Go 1.26.1's `fmt` on types with the same fields: `CMD ["/bin/sh" "-c" "echo hi"]`, `ENTRYPOINT []`, `VOLUME [/data /var/log]` (probed). BuildKit's own test expects `EXPOSE [1234/udp 2375/tcp 5000/tcp]` and `ARG foo=bar` [bk: df/dockerfile_history_test.go:135, 193].
- image-spec's example `/bin/sh -c #(nop) CMD ["sh"]` is the legacy builder's form [is: config.md:303-312]; BuildKit never writes it (derived).

**FROM**

- **Syntax.** One or three words, else `FROM requires either one or three arguments`. A blank expansion is `base name (%s) should not be blank` [bk: ins/parse.go:395-434; d2l/convert.go:412-414].
- **Resolution, in order:** a named context of that name (§2.7); an earlier stage of that name; `scratch`; then the registry [bk: d2l/convert.go:598-605, 666-729, 1249-1258].
  - A registry reference is normalized (`docker.io/library/…:latest`) and pinned to the digest it resolved to [bk: d2l/convert.go:691-729].
  - `--pull` sends `image-resolve-mode=pull`; without it, buildx with the docker driver sends `local`, which prefers an image already in the store [buildx: build/opt.go:580-585; bk: util/resolver/pool.go:253-274].
- **Inheritance.** The whole base config becomes the stage's, `created` cleared [bk: d2l/convert.go:713-740]. Its `Env` seeds the environment, and its `WorkingDir` and `User` apply without a history entry or a mkdir [bk: d2l/convert.go:804-821].
- **`scratch`** has `WorkingDir` `/`, the default `PATH`, and no history [bk: d2l/image.go:40-56].
- **`PATH`** is appended to the config of any non-Windows stage whose base lacks one, so the built image gains it [bk: d2l/convert.go:792-802].

**ARG**

- **Global ARGs** (before the first FROM) expand only in FROM and in later global defaults. A stage sees one only after `ARG name` without a value [bk: ins/parse.go:191-196; d2l/convert.go:1684-1691, 2146-2184].
- **A stage's value** is the `--build-arg` if given, else the expanded default, else the global value, else unset. A default is not evaluated when a build arg is given, so `${FOO:?…}` fails only without one [bk: d2l/convert.go:1677-1705; df/dockerfile_args_test.go:53-91].
- **Child stages** inherit the parent stage's ARGs [bk: d2l/convert.go:1217, 1227].
- **Undeclared build args** reach nothing, except the proxy names. BuildKit prints no "not consumed" warning (derived by grep); the legacy builder did: `[Warning] One or more build-args %v were not consumed` [moby: daemon/builder/dockerfile/buildargs.go:67-82].
- **Proxy args** (`HTTP_PROXY`, `HTTPS_PROXY`, `FTP_PROXY`, `NO_PROXY`, `ALL_PROXY`, any case) reach every RUN in both cases, stay out of the cache key, and stay out of history unless declared with ARG [bk: d2l/convert.go:1876-1908; ops/exec.go:132, 486-488, 573-591; ref.md:2684-2711].
- **Automatic args** in global scope [bk: d2l/platform.go:39-67]: `BUILDPLATFORM`, `BUILDOS`, `BUILDOSVERSION`, `BUILDARCH`, `BUILDVARIANT`; `TARGETPLATFORM`, `TARGETOS`, `TARGETOSVERSION`, `TARGETARCH`, `TARGETVARIANT`; `TARGETSTAGE`, the target's name, else the last stage's, else `default`. A build arg of the same name overrides each.
- **`BUILDKIT_*` args the frontend reads:** `BUILDKIT_SYNTAX` (§2.2), `BUILDKIT_MULTI_PLATFORM`, `BUILDKIT_SANDBOX_HOSTNAME`, `BUILDKIT_CACHE_MOUNT_NS`, `BUILDKIT_DOCKERFILE_CHECK`, `BUILDKIT_CONTEXT_KEEP_GIT_DIR`, the SBOM scan args, and `SOURCE_DATE_EPOCH` [bk: ui/config.go:55-60, 228-242, 299-321; ui/context.go:87; d2l/convert.go:304-311, 859-876]. `BUILDKIT_INLINE_CACHE` is read by buildx and dockerd, which turn it into an inline cache export [buildx: build/opt.go:261-269; moby: bn/builder.go:419-426].

**WORKDIR, USER and the rest**

- **WORKDIR** joins a relative path to the current one with `path.Join`; an absolute path is kept as written [bk: util/system/path.go:34-85, 108-121] (derived). Unless it is `/`, it adds a FileOp `mkdir -p`, 0755, owned by the current USER, timestamped `SOURCE_DATE_EPOCH` if set [bk: d2l/convert.go:1542-1564].
  - Only missing directories are created; existing ones are left alone [bk: solver/llbsolver/file/backend.go:28-66; V/github.com/tonistiigi/fsutil/copy/mkdir.go:12-20, 64-80].
  - A user name is looked up in the stage's `/etc/passwd` and `/etc/group`; an unknown one silently gives 0:0 [bk: solver/llbsolver/ops/user_linux.go:23-119] (derived).
- **USER** is stored unchecked. RUN checks it: `unable to find user X: no matching entries in passwd file` [bk: executor/oci/user.go:21-72].
- **EXPOSE** takes `[ip:][hostport:]port[-end][/tcp|udp|sctp]`; a range gives one key per port [bk: d2l/convert_expose.go:88-211].
- **STOPSIGNAL** accepts a nonzero number, or a name with or without `SIG` in any case: `invalid signal: X` [bk: V/github.com/moby/sys/signal/signal.go:38-51].
- **HEALTHCHECK.**
  - `NONE` gives `Test: ["NONE"]`.
  - `CMD` in JSON form gives `["CMD", …]`, in shell form `["CMD-SHELL", "<text>"]` [bk: ins/parse.go:592-636].
  - Unset durations and retries are stored as 0 and omitted; dockerd applies 30 s, 30 s, 0 s, 5 s and 3 retries at run time [bk: ins/parse.go:570-590, 638-676; moby: daemon/health.go:25-41].
- **ENTRYPOINT clears an inherited CMD.** The "CMD was set" tracker is per stage, so a CMD from the base image or a parent stage is dropped [bk: d2l/convert.go:1611-1627].
- **ONBUILD** stores the trigger's text; `ONBUILD ONBUILD`, FROM and MAINTAINER are refused as triggers [bk: ins/parse.go:436-468].
  - A child stage parses each trigger as one instruction and runs it first. Triggers come from the base image's config, or from the parent stage.
  - `OnBuild` is then cleared, so grandchildren never see them [bk: d2l/convert.go:548-586, 1209-1228, 1286-1329].

### 2.5 Q2: RUN

**The command**

- Shell form: `Config.Shell` or `/bin/sh -c`, plus the text (§2.2). Exec form: the array as written [bk: ins/parse.go:485-505].
- **Flags:** `--mount` and `--device` (repeatable), `--network`, `--security` [bk: ref.md:711-716]. The labs and mainline tag lists of 1.27 are both empty, so every flag here is in the stable frontend [bk: df/cmd/dockerfile-frontend/Dockerfile:17-21, 55] (derived).
- **Failure:** `process "<args joined by spaces>" did not complete successfully: exit code: N` [bk: ops/exec.go:554-556; frontend/gateway/pb/exit.go:37-42].

**The environment a RUN sees**

| What | Value | Source |
|---|---|---|
| `Env` | the base image's `Env`; then ENV and valued ARGs in order, a redefined name moving to the end | [bk: d2l/convert.go:804-808, 1340, 1708-1713; client/llb/meta.go:420-463] |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` if unset | [bk: d2l/convert.go:792-808; ops/exec.go:489-502] |
| proxy variables | from build args, in both cases | [bk: ops/exec.go:486-488, 573-591] |
| user | `USER`, else root; names from the rootfs's `/etc/passwd` and `/etc/group` (10 MiB each at most), supplementary groups including the primary | [bk: executor/oci/user.go:19-54, 130-157] |
| working directory | `WORKDIR`; an inherited one that is missing is made 0755 by the runc executor | [bk: executor/runcexecutor/executor.go:325-333] |
| hostname | `buildkitsandbox`, or `BUILDKIT_SANDBOX_HOSTNAME`; no `HOSTNAME` variable and no `/etc/hostname` | [bk: executor/oci/hosts.go:15; executor/oci/spec.go:144-147] (derived by grep) |
| stdin | none | [bk: ops/exec.go:525-530] |

- **`/etc/hosts`** is generated: `127.0.0.1\tlocalhost buildkitsandbox\n::1\tlocalhost ip6-localhost ip6-loopback\n`, plus a line per `--add-host` [bk: executor/oci/hosts.go:17-90].
- **`/etc/resolv.conf`** is derived from the daemon host's: loopback nameservers dropped (except with host networking), and Google's public servers when none remain [bk: executor/oci/resolvconf.go:32-158; util/resolvconf/resolvconf.go:32-43, 210-227].
- **Both are read-only bind mounts** over the image's own files [bk: executor/oci/spec_linux.go:44-52, 232-242]. So a RUN cannot change them, and they never reach a layer: afterwards, `MountStubsCleaner` removes each of `/etc/hosts`, `/etc/resolv.conf` and every mount target that did not exist before the run and is still empty, and restores its parent's timestamps [bk: executor/stubs.go:49-139].
- **The container** has containerd's default spec: 14 capabilities, the usual masked and read-only `/proc` paths, a `/dev` tmpfs, devpts, a 64 MiB `/dev/shm`, mqueue and read-only sysfs [bk: V/github.com/containerd/containerd/v2/pkg/oci/spec.go:118-223; V/github.com/containerd/containerd/v2/pkg/oci/mounts.go:25-70].
  - BuildKit removes the `/run` tmpfs, so writes under `/run` land in the layer [bk: executor/oci/spec_linux.go:244-254].
  - It allows new privileges, drops rlimits unless `--ulimit` is given, and applies the default seccomp profile [bk: executor/oci/spec.go:157-187; executor/oci/spec_linux.go:55-97].
- **The output** is the root mount alone, committed after a successful run [bk: ops/exec.go:389-392, 535-553]. Its diff is the layer (§2.9).

**`--mount`** [bk: ins/commands_runmount.go:133-310; d2l/convert_runmount.go:64-149]

| Type | Defaults | What it is |
|---|---|---|
| `bind` (default type) | the context, or `from=` a stage, image or named context; read-only | `rw` writes are discarded (`ForceNoOutput`) |
| `cache` | `from` scratch, `sharing=shared`, `id` = the cleaned target, prefixed by `BUILDKIT_CACHE_MOUNT_NS` | a persistent directory per builder, never in the image; `private` makes another copy when busy, `locked` waits [bk: solver/llbsolver/mounts/mount.go:68-154] |
| `tmpfs` | `size` unlimited | `nosuid` tmpfs [bk: solver/llbsolver/mounts/mount.go:327-342] |
| `secret` | `id` = `source`, `id` or the target's base name; target `/run/secrets/<id>`; mode 0400, 0:0; optional unless `required` | one file on a private tmpfs, bind-mounted read-only; `env=NAME` puts it in the environment instead or too [bk: d2l/convert_secrets.go:14-90; client/llb/exec.go:779-787; ops/exec.go:498-502] |
| `ssh` | id `default`; target `/run/buildkit/ssh_agent.<N>`; mode 0600 | a forwarded agent socket; `SSH_AUTH_SOCK` set to the first unless already set [bk: client/llb/exec.go:155-164, 710-720] |

- A relative target joins the working directory; `/` is `invalid mount target` [bk: d2l/convert_runmount.go:121-131].
- `from=` refuses variables (§2.3).

**`--network`, `--security`, `--device`**

- `--network=default|none|host`. `default` inherits the build's mode, which is the sandbox's own network [bk: ins/commands_runnetwork.go:9-54; d2l/convert_runnetwork.go:11-24]. dockerd's embedded BuildKit puts that on a libnetwork bridge [moby: bn/executor_linux.go:27-31].
- `--security=sandbox|insecure`. `insecure` adds every capability and clears the masked paths [bk: util/entitlements/security/security_linux.go:20-48].
- `host` and `insecure` need entitlements (`--allow`), else `network.host is not allowed` or `security.insecure is not allowed` [bk: solver/llbsolver/vertex.go:129-139; util/entitlements/entitlements.go:156-169].
- `--device=name[,required]` names a CDI device [bk: d2l/convert_rundevice.go:8-20].

**Where it runs.** dockerd's BuildKit runs every ExecOp with runc: `runc run` in a fresh bundle over the step's writable snapshot [moby: bn/executor_linux.go:56-60, 80-92; bk: executor/runcexecutor/executor.go:246-360].

### 2.6 Q2: COPY and ADD

**Flags** [bk: ins/parse.go:316-393; ref.md:1365-1373, 1688-1695]

| Flag | COPY | ADD | Since | Notes |
|---|---|---|---|---|
| `--from` | yes | no | — | a stage name (any case), a 0-based index, a named context, then an image; no variables [bk: d2l/convert.go:942-980, 1120-1131, 1261-1272] |
| `--chown` | yes | yes | — | `user[:group]`; names looked up in the destination stage's `/etc/passwd` and `/etc/group`; unknown names give 0 silently (§2.14) [bk: client/llb/fileop.go:231-266, 770-790; solver/llbsolver/ops/user_linux.go:23-119] |
| `--chmod` | yes | yes | 1.2 | octal up to 07777, or symbolic (1.14); applies to directories too; else `invalid chmod parameter: '64a'. it should be octal string and between 0 and 07777` [bk: d2l/convert_copy.go:65-85] |
| `--link` | yes | yes | 1.4 | copies onto scratch and merges; off when `--chmod` is given [bk: d2l/convert_copy.go:334-352] |
| `--exclude` | yes | yes | 1.19 | patternmatcher patterns, never expanded [bk: V/github.com/tonistiigi/fsutil/copy/copy.go:295-311, 337-377] |
| `--parents` | yes | no | 1.20 | keeps the path below `/./`; `**` allowed [bk: d2l/convert_copy.go:235-270] |
| `--checksum` | no | yes | 1.6 | one source, HTTP(S) or git; HTTP takes `sha256:` only [bk: d2l/convert_copy.go:87-97; ref.md:1629-1632] |
| `--keep-git-dir`, `--unpack` | no | yes | —, 1.17 | git's `.git` kept; archives unpacked or not [bk: ins/parse.go:316-360] |

- Fewer than two words: `COPY requires at least two arguments, but only one was provided. Destination could not be determined` [bk: ins/parse.go:814-816].
- A stage defined later: `cannot copy from stage %q, it needs to be defined before current stage %q` [bk: d2l/convert.go:1121-1128].

**Sources** [bk: d2l/convert_copy.go:231-294]

- Each source is made absolute from the source's root and cleaned, so `../` goes and trailing slashes are dropped [bk: ref.md:1423-1427].
- The copy follows a named symlink (the link's name is kept) and copies a directory's contents, not the directory. Symlinks inside a copied directory stay symlinks [bk: V/github.com/tonistiigi/fsutil/copy/copy.go:30-44, 448-455; df/dockerfile_copy_test.go:639-705].
- **Wildcards.** The path is split at the first component holding `*`, `?` or `[`; the rest is matched with Go's `filepath.Match`, with no `**` [bk: V/github.com/tonistiigi/fsutil/copy/copy.go:46-67, 687-733]. No match is no error: the copy does nothing [bk: solver/llbsolver/file/backend.go:238-254].
- **A missing plain source** fails while the cache key is computed: `failed to compute cache key: failed to calculate checksum of ref <id>: "/x": not found` (derived from [bk: solver/edge.go:916; ops/opsutils/contenthash.go:59-61; cache/contenthash/checksum.go:31, 895]).

**Destination**

- Absolute, or joined to WORKDIR keeping a trailing slash; `.` is `./` [bk: d2l/convert.go:1731-1752; solver/llbsolver/file/backend.go:427-439].
- Missing parents are created 0755, **or with the `--chmod` mode**, owned by the `--chown` owner or 0:0 [bk: V/github.com/tonistiigi/fsutil/copy/copy.go:76-94; V/github.com/tonistiigi/fsutil/copy/mkdir.go:12-80; df/dockerfile_copy_test.go:783, 842-844].
- A file onto an existing directory goes inside it; a file replaces a file; a file onto a directory is `cannot replace to directory %s with file`, a directory onto a file `cannot copy to non-directory: %s` [bk: V/github.com/tonistiigi/fsutil/copy/copy.go:146-180, 641-672].
- Merging into an existing directory gives its subdirectories the source's mode, owner and times; the top-level destination keeps its mode and owner and takes the source's times [bk: V/github.com/tonistiigi/fsutil/copy/copy.go:417-476, 568-639] (derived).

**Metadata**

- **The context arrives as 0:0.** Clients reset every context file's owner before sending, and keep mode, mtime (ns), device numbers, symlink targets, hardlinks and xattrs other than `com.apple.*` [buildx: build/build.go:1184-1207; bk: client/solve.go:489-494; V/github.com/tonistiigi/fsutil/stat.go:18-56; V/github.com/tonistiigi/fsutil/stat_unix.go:15-76].
- **A Windows client** sends no owner or xattrs, and every mode as its permission bits with `0111` added and masked to `0755` [bk: V/github.com/tonistiigi/fsutil/stat.go:43-50; V/github.com/tonistiigi/fsutil/stat_windows.go:11-16].
- **The copy keeps** the source's owner unless `--chown` (so `--from` keeps the stage's owners), its mode unless `--chmod`, its atime and mtime (COPY sets no timestamp), its xattrs (failures only logged), and hardlinks within one source; devices and FIFOs are recreated [bk: solver/llbsolver/file/backend_unix.go:13-45; V/github.com/tonistiigi/fsutil/copy/copy_linux.go:18-74; V/github.com/tonistiigi/fsutil/copy/copy_nowindows.go:15-45; V/github.com/tonistiigi/fsutil/copy/hardlink.go:16-27; d2l/convert_copy.go:277-287].
- **Heredoc files** are 0644, created on scratch at build time [bk: d2l/convert_copy.go:297-321].

**ADD's other sources**

- **HTTP(S)** URLs not ending in `.git` [bk: d2l/convert_copy.go:357-370]. COPY refuses them: `source can't be a URL for COPY` [bk: d2l/convert_copy.go:188-190].
  - The file is named after the URL path's last component, else `__unnamed__`; mode 0600, 0:0; mtime from `Last-Modified`, else 1970-01-01. It is unpacked only with `--unpack=true` [bk: d2l/convert_copy.go:197-224; source/http/source.go:758-817, 987-1009].
- **Git** (`git@…`, `ssh://`, `git://`, or HTTP(S) ending `.git`) with `#ref:subdir` and query options. Submodules are fetched; `.git` is dropped unless `--keep-git-dir`; files are 0644 and 0755 [bk: df/dfgitutil/git_ref.go:71-242; source/git/source.go:1287-1292; source/git/source_linux.go:13-33]. COPY refuses them: `source can't be a git ref for COPY`.
- **Local archives** are unpacked into the destination when the content, not the name, is a tar, plain or gzip, bzip2, xz or zstd compressed. Owners in the archive are kept unless `--chown`; `--chmod` is not applied [bk: solver/llbsolver/file/unpack.go:31-84; V/github.com/moby/go-archive/compression/compression_detect.go:14-17].
  - xz needs an `xz` binary; if it cannot run, the build fails rather than copying the file [bk: V/github.com/moby/go-archive/compression/compression.go:194-198; solver/llbsolver/file/unpack.go:73-79].

**What executes where**

- One COPY or ADD is one FileOp of chained copy actions, run by the daemon itself through fsutil; no container is involved [bk: client/llb/fileop.go:566-629; solver/llbsolver/file/backend.go:272-287]. HTTP and git fetches are sources, also run by the daemon.
- `created_by` is `COPY|ADD [--parents] [--chown=…] [--chmod=…] <srcs> [<<NAME…] <dest> # buildkit`; `--from`, `--link`, `--exclude`, `--checksum`, `--keep-git-dir` and `--unpack` leave no trace [bk: d2l/convert_copy.go:99-128, 323].
- `COPY --link` makes its layer as a diff against scratch, so it has no whiteouts and does not depend on the base [bk: d2l/convert_copy.go:334-349; docs/dev/merge-diff.md:85-95, 274-289] (inference).

### 2.7 Q3: multi-stage builds, `--target` and platforms

- **Order.** Parse; name the stages; pick the target; build the dependency graph; keep what is reachable; resolve bases; dispatch in file order [bk: d2l/convert.go:273-359].
- **The target** is `--target`, lower-cased, or the last stage. An unknown one is `target stage "x" could not be found`, with ` (did you mean y?)` when close [bk: d2l/convert.go:507-516, 1261-1276; util/suggest/error.go:9-35].
- **What is built:** the target, its base chain, and every stage reached through `COPY --from`, `RUN --mount from=` or ONBUILD triggers, transitively [bk: d2l/convert.go:527-534, 1830-1847]. Other stages are neither dispatched nor resolved [bk: d2l/convert.go:588-617, 778-783]. The legacy builder built every stage up to the target [ddocs: B/building/multi-stage.md:143-168].
- **References.** FROM names an earlier stage. `COPY --from` takes a name, a 0-based index, a named context or an image; `RUN --mount from=` takes a name or an image [bk: d2l/convert.go:942-973; d2l/convert_runmount.go:16-46]. A cycle is `circular dependency detected on stage: %s` [bk: d2l/validations.go:82-119].
- **Duplicate stage names** only warn (`DuplicateStageName`); FROM binds the latest earlier definition, while `--from` and `--target` bind the last (derived) [bk: d2l/validations.go:29-32, 172-188].
- **Named contexts** (`--build-context name=…`): docker-image, git, HTTP(S), OCI layout or a local directory. One named like a stage replaces the stage, whose instructions do not run [bk: ui/namedcontext.go:23-307; d2l/convert.go:460-475, 641-663].
- **History is per stage.** A stage that copies from another gets only the COPY's entry [bk: df/dockerfile_history_test.go:46-58, 100-111].
- **Platforms.**
  - `--platform` sets the target platforms; several mean one conversion each, in parallel [bk: ui/config.go:171-180; ui/build.go:30-104].
  - The build platform is the worker's; without `--platform`, the target is the build platform [bk: ui/config.go:164-170; d2l/platform.go:15-37].
  - `FROM --platform` picks the base's platform and the stage's. It is ignored for another stage (the parent's wins) and for scratch [bk: d2l/convert.go:428-458, 598-605, 1217-1218] (derived).
  - The final config's `os` and `architecture` are the target's when one was given, else the base's [bk: d2l/convert.go:923-937] (derived).
  - arm64's variant `v8` normalizes to empty, so `TARGETPLATFORM` is `linux/arm64` [bk: V/github.com/containerd/platforms/database.go:76-111] (derived).
  - A RUN for another architecture runs under QEMU user-mode emulation [bk: docs/multi-platform.md:19-20; ops/exec.go:451-464].

### 2.8 Q4: the build context and `.dockerignore`

**How it travels**

- The frontend reads two local sources from the client, `context` and `dockerfile` [bk: ui/context.go:28-34].
  - `dockerfile` holds the Dockerfile and `<name>.dockerignore`, fetched with `FollowPaths` and no differ: progress `[internal] load build definition from Dockerfile` [bk: ui/config.go:334-413].
  - `.dockerignore` is read from `context` alone: `[internal] load .dockerignore` [bk: ui/config.go:537-574].
  - `context` is fetched with the ignore patterns as `ExcludePatterns`, narrowed by `FollowPaths` to the sources of COPY, ADD and RUN bind mounts without `from=`. A source of `/` (`COPY . .`) sends everything [bk: ui/config.go:444-474; d2l/convert.go:901-913, 1092-1097, 1146-1150, 1849-1874].
- **The client** filters and streams over the session's gRPC `diffcopy` protocol [bk: session/filesync/filesync.go:26-29, 81-138, 184-210].
- **The daemon** keeps one snapshot per context, named by the client's shared key, and re-syncs it incrementally, comparing size, mtime, mode, owner, device and link target [bk: source/local/source.go:204-312; client/llb/source.go:931-941]. buildx's shared key is the context directory's base name [buildx: build/opt.go:891-900].
- **The Dockerfile-specific ignore file:** `<Dockerfile name>.dockerignore` beside the Dockerfile wins over the context's `.dockerignore` [bk: ui/config.go:417-423, 540-574; ddocs: B/concepts/context.md:488-509].
- **Other contexts** [buildx: build/opt.go:850-966; bk: ui/context.go:95-149, 295-364]:
  - `-`: a tar on stdin (plain or compressed) is uploaded; anything else is the Dockerfile, with an empty context. `-f -` with context `-` is `can't use stdin for both build context and dockerfile`.
  - Git URLs become a git source; HTTP URLs are fetched, and unpacked if they are archives, else used as the Dockerfile.
  - No `.dockerignore` applies to them [bk: ui/config.go:509-522] (inference).

**The ignore file** [bk: V/github.com/moby/patternmatcher/ignorefile/ignorefile.go:24-73]

- BOM stripped. A comment only when `#` is in column 1: `  # x` is the pattern `# x` (probed).
- Each line is trimmed; a leading `!` inverts; then `filepath.Clean`, `/` separators, one leading `/` dropped. A bare `!` is `illegal exclusion pattern: "!"` [bk: V/github.com/moby/patternmatcher/patternmatcher.go:54-57].

**Matching** [bk: V/github.com/moby/patternmatcher/patternmatcher.go:42-76, 128-163, 216-272, 312-440]

- Patterns are anchored at the context root, and a match on any parent counts: `foo` excludes `foo/bar`, not `a/foo`.
- `*` is `[^/]*`, `?` is `[^/]`, `**` is `(.*/)?`; `[…]` passes through; `\` escapes on Unix. Syntax is checked with `filepath.Match(p, ".")`, so `[a` is `syntax error in pattern`.
- The last matching line wins: an exclusion applies only while the path is included, and an exception only while it is excluded (probed; it reproduces the reference's examples [ddocs: B/concepts/context.md:535-616]).
- The walk descends into an excluded directory when an exception could re-include something below it, emitting re-included files' parents lazily [bk: V/github.com/tonistiigi/fsutil/filter.go:77-148, 254-292, 336-374].
- patternmatcher v0.6.1 is byte-identical in BuildKit's, the CLI's, moby's and buildx's vendor trees (derived by `cmp`).

**The legacy client, for contrast** [cli: I/build.go:231-324]

- It tars the context itself, owners 0:0, printing `Sending build context to Docker daemon`. It adds `!.dockerignore` and `!<Dockerfile>` so both always travel, and the daemon deletes them if excluded [cli: I/build/dockerignore.go:14-49; moby: daemon/builder/remotecontext/detect.go:116-140]. BuildKit's separate `dockerfile` source gives the same effect: "still sent" [ddocs: B/concepts/context.md:574-577].

### 2.9 Q5: the image produced

**The config**

- **Written by** the frontend as a `DockerOCIImage`, the OCI image plus Docker's `Healthcheck`, `OnBuild` and `Shell` [bk: V/github.com/moby/docker-image-spec/specs-go/v1/image.go:11-54], marshalled with `json.Marshal` [bk: ui/build.go:54].
- **Patched by** the exporter, which re-parses it into a map and replaces `rootfs` (`{"type":"layers","diff_ids":[…]}`, from each layer's uncompressed digest), `history`, and `created` if missing [bk: exporter/containerimage/writer.go:748-841].
- **The bytes** (probed, with a harness that reproduces the layout):
  - compact JSON, top-level keys in byte order, because Go sorts map keys: `architecture`, `config`, `created`, `history`, `os`, `rootfs`, then `variant` [go: src/encoding/json/encode.go:765-796];
  - `config` in struct order: `User`, `ExposedPorts`, `Env`, `Entrypoint`, `Cmd`, `Volumes`, `WorkingDir`, `Labels`, `StopSignal`, `ArgsEscaped`, `Healthcheck`, `OnBuild`, `Shell` [bk: V/github.com/opencontainers/image-spec/specs-go/v1/config.go:23-111];
  - maps inside it sorted; `<`, `>` and `&` escaped as `\u003c`, `\u003e` and `\u0026` [go: src/encoding/json/encode.go:205-209, 473-493].
- **`--label`** values merge into the final `Labels` without a history entry [bk: d2l/convert.go:887-891].

**History and time**

- **Fields.** image-spec defines `created`, `created_by`, `author`, `comment` and `empty_layer`, the last true when an entry made no layer [is: config.md:224-249].
- **Entries** as §2.4 lists them. RUN's `created_by` is `RUN ` plus the executed arguments joined by spaces, preceded by `|N k=v …` when the stage has declared ARGs, plus ` # buildkit`: `RUN /bin/sh -c apk add curl # buildkit` [bk: d2l/convert.go:1507, 1794-1814]. BuildKit's test expects `RUN |2 foo=bar2 bar=123 ` [bk: df/dockerfile_history_test.go:204-208].
- **Reconciled with the layers** by `normalizeLayersAndHistory`: extra non-empty entries are marked empty from the bottom, missing ones appended with comment `buildkit.exporter.image.v0` [bk: exporter/containerimage/writer.go:843-928].
- **Without `SOURCE_DATE_EPOCH`**, a layer's entry gets its snapshot's creation time, a metadata entry the time of the next or previous layer, and the config's `created` the last entry's [bk: exporter/containerimage/writer.go:817-829, 843-928; cache/manager.go:1619]. So a fully cached rebuild reproduces the config's bytes (inference).
- **With `SOURCE_DATE_EPOCH`** (a build arg; buildx passes the environment variable [buildx: C/build.go:150-154]) [bk: docs/build-repro.md:40-76; d2l/epoch.go:34-111]:
  - new history entries carry the epoch, and the exporter clamps every entry after the base's, and `created`, to it [bk: exporter/containerimage/writer.go:781-815];
  - WORKDIR's mkdir is stamped with it, so changing it invalidates WORKDIR and everything after [bk: d2l/convert.go:1549-1551; ddocs: B/cache/invalidation.md:47-51];
  - file times change only with the exporter option `rewrite-timestamp=true`, which re-streams each layer after the base's, clamping mtimes to the epoch [bk: exporter/containerimage/writer.go:439-487; util/converter/tarconverter/tarconverter.go:14-58].
  - `rewrite-timestamp` conflicts with `unpack` [bk: exporter/containerimage/export.go:325-331], which moby's exporter turns on for the containerd store [moby: bn/exporter/wrapper.go:57-59]. So `docker build --load` cannot rewrite timestamps there, while `-o type=oci` can (inference).
- **COPY keeps source mtimes.** No timestamp is set, and the context carries the host's mtimes [bk: client/llb/fileop.go:685-690; solver/llbsolver/file/backend.go:20-26, 216].

**How a layer is computed** [bk: cache/blobs.go:96-274; cache/blobs_linux.go:28-129]

- The overlay differ runs when the snapshotter is overlayfs; otherwise, or on failure, the walking (double-walk) differ. The DiffID is the SHA-256 of the uncompressed stream, teed before compression.
- **The overlay differ** [bk: util/overlay/overlay_linux.go]:
  - It takes the top upper directory as the diff, and accepts only the mount options `userxattr`, `index=off`, `redirect_dir=…` and `workdir`; any other falls back to the walking differ [bk: util/overlay/overlay_linux.go:27-109].
  - It walks the upper directory in lexical order [bk: util/overlay/overlay_linux.go:113-244]:
    - a directory with `trusted.overlay.redirect` is an error, `redirect_dir is used but it's not supported in overlayfs differ` [bk: util/overlay/overlay_linux.go:179-186];
    - a 0/0 character device is a delete if the path exists in the lower, and is skipped if it does not [bk: util/overlay/overlay_linux.go:247-269];
    - an opaque directory (`trusted.overlay.opaque` or `user.overlay.opaque` = `y`) that exists in the lower is diffed again against it, emitting explicit whiteouts, never `.wh..wh..opq` [bk: util/overlay/overlay_linux.go:227-241, 272-292];
    - a path in the lower is a change unless mode, owner, rdev and `security.capability` are equal, and, for non-directories, size and mtime; equal whole-second mtimes fall back to comparing contents. Directories never compare mtime [bk: util/overlay/overlay_linux.go:203-212, 318-359].
  - So the differ reads the lower as well as the upper (derived).
- **The tar writer**, containerd's `ChangeWriter` [bk: ctrd-archive/tar.go:505-732]:
  - **Deletes** become `dir/.wh.<name>`: a regular file of size 0, mode 0, owner 0:0, mtime the Unix epoch [bk: ctrd-archive/tar.go:548-568].
  - **Other entries** take uid, gid and mode from stat, no user or group names, device numbers only for devices, and names relative with `/`, directories ending in `/` [bk: ctrd-archive/tar.go:585-615; V/github.com/containerd/containerd/v2/pkg/archive/tarheader/tarheader.go:73-82].
  - **Times:** PAX format requested, mtime truncated to whole seconds, atime and ctime dropped [bk: ctrd-archive/tar.go:591-600]. Go then writes a plain USTAR header unless a PAX record is needed [go: src/archive/tar/common.go:396-423, 518-523] (probed).
  - **Xattrs:** only `security.capability`, as `SCHILY.xattr.security.capability` [bk: ctrd-archive/tar.go:645-652]. So `user.*`, `trusted.*` (overlay's own included) and `security.selinux` never reach a layer (derived).
  - **Parents:** before each entry, every ancestor not yet written, except the root, is written with its stat in the merged view [bk: ctrd-archive/tar.go:654, 705-732].
  - **Hardlinks:** the first path carries the data; later ones link to it [bk: ctrd-archive/tar.go:621-643, 677-691]. Sockets are skipped [bk: ctrd-archive/tar.go:577-578].
- **Stubs.** After a RUN, mount stubs that are still empty are removed first (§2.5).
- **Against image-spec:** explicit whiteouts are a SHOULD, and readers MUST accept opaque ones too [is: layer.md:315]. Layers MUST carry xattrs "where supported" [is: layer.md:48-60]; BuildKit keeps one (derived).
- **Which instructions make layers:** RUN, COPY, ADD and WORKDIR other than `/`. No code drops an empty diff (derived by grep), so a WORKDIR that already exists would give an empty layer: UNVERIFIED.

**Compression, media types, the ID**

- **Blobs** default to gzip, Go's `compress/gzip` at its default level [bk: util/compression/compression.go:78; util/compression/gzip.go:58-66]. With the default compatibility version, media types are OCI's (`application/vnd.oci.image.layer.v1.tar+gzip`) [bk: exporter/containerimage/export.go:586-592; exporter/containerimage/writer.go:513-523].
- **Default provenance.** With a daemon that supports attestations and the docker driver, buildx adds `attest:provenance=mode=min,inline-only=true`, unless `--provenance` or `BUILDX_NO_DEFAULT_ATTESTATIONS` [buildx: build/opt.go:349, 362-373]. Provenance records start and finish times [bk: solver/llbsolver/provenance.go:445-448], so the index digest changes with every build (inference).
- **Engine 29's default store is containerd's**, unless a graphdriver already holds data [moby: daemon/image_store_choice.go:94-153].
  - There the image is an OCI index of the image manifest and the attestation manifest, and its ID is the index's digest [bk: exporter/containerimage/writer.go:87-104, 235-379; moby: daemon/containerd/image.go:62].
  - On a graphdriver store, the ID is the config's SHA-256, printed as `writing image sha256:<config digest>` [moby: bn/exporter/mobyexporter/export.go:139-213], image-spec's ImageID [is: config.md:85-89].
- **So the stable things to compare** with a real build are the config's bytes, its history and the DiffIDs. Compressed digests, manifests, indexes and containerd-store IDs differ by construction (derived).

### 2.10 Q6: caching

The model (cache maps, keys chained from inputs, content-based selectors, cache-fast before cache-slow) is in container-engine-internals.md §2.6. What each op puts in its key:

- **RUN** [bk: ops/exec.go:114-186; solver/pb/ops.proto:45-69]:
  - in: args, env, cwd, user, hostname, ulimits, extra hosts (names, not IPs), mounts (without selectors), network and security modes, secret-env names, CDI devices, and the platform;
  - out: proxy variables [bk: ops/exec.go:132], a cache mount's custom ID and sharing mode [bk: ops/exec.go:93-131], and resource limits [bk: docs/dev/solver.md:212-215];
  - read-only mounts, such as `--mount=type=bind`, also key on their content; secret, SSH and tmpfs mounts on nothing [bk: ops/exec.go:294-334]. "The contents of build secrets are not part of the build cache" [ddocs: B/cache/invalidation.md:90-111].
- **COPY, ADD, WORKDIR's mkdir** [bk: ops/file.go:55-170, 221-231]: the actions, with each copy's source reduced to its base name; the full source paths become the selector, whose content is hashed with the copy's wildcard, follow, include, exclude and required-path options.
- **The content hash** [bk: cache/contenthash/tarsum.go:38-75; cache/contenthash/filehash.go:17-70; cache/contenthash/checksum.go:888-958, 1225-1243]:
  - a file: SHA-256 of a tar-header record (mode, uid, gid, size, type, link name, device numbers, and sorted xattrs other than `security.*` and `system.*`, keeping `security.capability`), then its bytes. **No mtime.**
  - a directory: SHA-256 over its sorted entries' names and digests, and its own record.
  - Docs agree: mtime "is not taken into account" [ddocs: B/cache/invalidation.md:24-32].
- **Sources:**
  - an image keys on its manifest digest and platform, then the ChainID of its DiffIDs [bk: source/containerimage/pull.go:65-87, 302-323];
  - the context keys on the session, so across builds COPY hits only by content [bk: source/local/source.go:129-162] (derived);
  - HTTP keys on name, mode, owner, content digest and `Last-Modified`, revalidated with ETags on every build unless `--checksum` pins it [bk: source/http/source.go:243-303, 368-440];
  - git keys on the resolved commit [bk: source/git/source.go:291-305, 776-797].
- **ARG and ENV** reach later RUN keys through the environment [bk: d2l/convert.go:1340, 1708-1711]. A build arg that is never declared enters no key (inference).
- **`--no-cache`** sends `no-cache=""`; `--no-cache-filter` names stages [buildx: build/opt.go:589-594]. The frontend marks RUN, COPY and ADD, not FROM, to skip lookups; their results are still stored [bk: ui/config.go:218-226, 497-507; d2l/convert.go:501, 1414-1416; solver/edge.go:196-202, 640-647].
- **`--pull`** resolves the base in the registry; without it the docker driver prefers the local store (§2.4).
- **`CACHED`** means the vertex was loaded from a record, not run [bk: solver/jobs.go:1036-1051, 1367-1380].
- **Garbage collection** under dockerd keeps 10% of the disk (2 GB if unreadable), at most 80% used, 20% free, and drops unused ephemeral records after 48 h and anything after 60 days [moby: bn/worker/gc.go:11-72; ddocs: B/cache/garbage-collection.md:103-111].

### 2.11 Q7: the command line

**`docker build` is buildx**

- The CLI hands `build` and `image build` to the buildx plugin before parsing any flag, so every flag and every error comes from buildx [cli: cmd/docker/builder.go:149-180].
- `DOCKER_BUILDKIT=0` still selects the legacy builder, with `DEPRECATED: The legacy builder is deprecated and will be removed in a future release.` [cli: cmd/docker/builder.go:26-28, 100-109].
- The CLI sets `BUILDX_BUILDER` to the current context unless given, so `docker build` uses the daemon's own BuildKit (the docker driver) [cli: cmd/docker/builder.go:131-141].
- The legacy builder's own flags, for contrast, are at [cli: I/build.go:96-169].

**Flags** (buildx v0.37.1, `build [OPTIONS] PATH | URL | -`, exactly one argument [buildx: C/build.go:513-548])

| Flag | Default and behaviour | Source |
|---|---|---|
| `-t`, `--tag` | repeatable; normalized to `docker.io/library/x:latest`; a digest is refused | [buildx: C/build.go:601; moby: bn/exporter/wrapper.go:47-55] |
| `-f`, `--file` | `PATH/Dockerfile`; relative to the current directory; `-` is stdin | [buildx: C/build.go:573; build/opt.go:891-909] |
| `--build-arg` | a bare `KEY` takes `$KEY` if set, else is dropped | [buildx: C/build.go:784-802] |
| `--target` | the last stage | §2.7 |
| `--platform` | `$DOCKER_DEFAULT_PLATFORM`; comma lists; `local` is the host | [buildx: C/build.go:550-553] |
| `--no-cache`, `--pull` | off; conflicts with `--no-cache-filter`; the docker driver prefers local bases | [buildx: C/build.go:1032-1035; build/opt.go:580-585] |
| `-q`, `--quiet` | off; conflicts with any `--progress` but `auto` and `quiet` | [buildx: C/build.go:226-242] |
| `--progress` | `auto` (TTY if stderr is a terminal), `plain`, `tty`, `quiet`, `none`, `rawjson`; `BUILDKIT_PROGRESS` overrides only `auto` | [buildx: C/build.go:685; util/progress/printer.go:130-132] |
| `--iidfile` | written 0644 without a newline; refused with local and tar outputs | [buildx: C/build.go:187-191, 342-347, 438-442] |
| `--label` | repeatable, "Set metadata for an image" | [buildx: C/build.go:577] |
| `--secret` | `id=`, `src=`/`source=`, `env=`, `type=file|env`; with `id` alone, the variable, else the file, of that name | [buildx: util/buildflags/secrets.go:91-110; bk: session/secrets/secretsprovider/store.go:24-30] |
| `--ssh` | `default` means `$SSH_AUTH_SOCK` | [buildx: C/build.go:1088-1091] |
| `--network` | `default`, `none` or `host` | [buildx: build/opt.go:615-624] |
| `-o`, `--output` | a bare path is `type=local`; `-` is a tar on stdout, refused if stdout is a terminal | [buildx: util/buildflags/export.go:107-116; build/opt.go:1366-1378] |
| `--load` | a no-op with the docker driver, which loads by default | [buildx: build/opt.go:385-402, 482-519] |
| `--rm`, `--force-rm`, `-m`, `--cpu-*`, `--isolation`, `--security-opt`, `--squash` | hidden legacy flags that still parse; some only warn | [buildx: C/build.go:618-669] |

**Output**

- **Progress goes to stderr** [buildx: C/build.go:376-377]. Plain mode, from BuildKit's printer [bk: pui/printer.go; pui/display.go:147-165]:
  - `#0 building with "<builder>" instance using <driver> driver`, then a blank line;
  - per vertex: a blank line, `#N <name>`, status lines `#N <id> 227B 0.0s done`, logs `#N 0.312 hello` (seconds since the step started), warnings `#N WARN: <Rule>: <msg> (line N)`, and one of `#N DONE 0.1s`, `#N CACHED`, `#N ERROR: …`, `#N CANCELED`;
  - for a failed step, `------`, ` > <name>:`, its last ten log lines, `------`.
- **Names.** `[<stage> <i>/<n>] <INSTRUCTION> <text>`, with `i` right-aligned to `n`'s width, `n` = ADD, COPY, RUN and WORKDIR plus one for a pulled FROM, and the stage part the `AS` name, `stage-<i>`, or nothing in a one-stage file [bk: d2l/convert.go:477-499, 542-544, 1952-1969].
- **Internal vertices:** `[internal] load build definition from Dockerfile`, `[internal] load metadata for docker.io/library/alpine:latest`, `[internal] load .dockerignore`, `[internal] load build context` [bk: ui/config.go:334-574; d2l/convert.go:691-701].
- **Export**, on the containerd store: `exporting to image`, `exporting layers`, `exporting manifest sha256:…`, `exporting config sha256:…`, with provenance `exporting attestation manifest sha256:…` and `exporting manifest list sha256:…`, then `naming to docker.io/library/x:latest` and `unpacking to docker.io/library/x:latest` [bk: exporter/containerimage/writer.go:371, 392, 558, 570, 672; exporter/containerimage/export.go:284, 471]. On a graphdriver store: `exporting layers`, `writing image sha256:…`, `naming to …` [moby: bn/exporter/mobyexporter/export.go:141, 195-214].
- **A probed run** of BuildKit's own printer on a synthetic build (times invented, format real):

  ```
  #6 [2/3] COPY app.txt /app/
  #6 CACHED

  #7 [3/3] RUN echo hello && exit 3
  #7 0.312 hello
  #7 ERROR: process "/bin/sh -c echo hello && exit 3" did not complete successfully: exit code: 3
  ------
   > [3/3] RUN echo hello && exit 3:
  0.312 hello
  ------
  Dockerfile:3
  --------------------
     1 |     FROM alpine
     2 |     COPY app.txt /app/
     3 | >>> RUN echo hello && exit 3
  ```

- **The error line** follows the snippet: `ERROR: failed to build: failed to solve: process "/bin/sh -c echo hello && exit 3" did not complete successfully: exit code: 3` (derived from [buildx: C/build.go:506; cmd/buildx/main.go:119-127; bk: client/solve.go:339; solver/errdefs/source.go:47-94]).
- **Warnings** end the output: ` N warning(s) found (use docker --debug to expand):` then ` - <short>` per warning, in yellow even when stderr is not a terminal [buildx: C/build.go:804-823] (derived).
- **`-q`** discards the progress and prints the image ID on stdout: the config digest if the exporter reports one, else the image digest [buildx: C/build.go:429-434, 468-475]. On a containerd store with default provenance that is the index's digest (derived, §2.9).

**Exit status** [buildx: cmd/buildx/main.go:119-147]

- 0 on success; 1 for a failed build; 100 for an internal error, 102 for resource exhaustion, 130 when cancelled.
- 125 for a flag error, with the usage [buildx: vendor/github.com/docker/cli/cli/cobra.go:79-88].
- The CLI passes the plugin's status through unchanged [cli: cmd/docker/docker.go:414-440].

### 2.12 Q8: building in VMs rather than containers

**Apple's `container build`: BuildKit in one builder VM, a runc container per RUN**

- **The builder** is one container, ID `buildkit`, and in `container` every container is its own VM [ctr: Sources/ContainerBuild/Builder.swift:30; docs/resource-usage.md:18].
  - Defaults: 2 CPUs, 2048 MB, Rosetta, image `ghcr.io/apple/container-builder-shim/builder:<version>` [ctr: Sources/ContainerPersistence/ContainerSystemConfig.swift:80-87], `linux/arm64` only [ctr: Sources/ContainerCommands/Builder/BuilderStart.swift:116].
  - It runs `container-builder-shim` as root with every capability [ctr: Sources/ContainerCommands/Builder/BuilderStart.swift:211-216, 249-281], and is reused across builds until its settings change [ctr: Sources/ContainerCommands/Builder/BuilderStart.swift:137-209].
- **Inside:** the image is moby/buildkit v0.26.2's five layers plus three of the shim's [assets; shim: Dockerfile:2, 33-39]. The shim writes a `buildkitd.toml` with the OCI worker on `/usr/bin/buildkit-runc`, starts `buildkitd` [shim: pkg/buildkit/config.go:26-44; pkg/buildkit/buildkit.go:28-44], and serves its own gRPC API on vsock port 8088 [shim: main.go:41-51, 130-140].
  - So each RUN is `runc run` in a fresh bundle, inside the one VM [bk: executor/runcexecutor/executor.go:259-264] (derived).
- **The frontend runs in the shim**, in process: it links BuildKit v0.29.0 and calls `parser.Parse`, `instructions.Parse` and `dockerfile2llb.Dockerfile2LLB` itself [shim: go.mod:13; pkg/build/frontend.go:115-121, 326-378]. A v0.29.0 frontend thus drives a v0.26.2 daemon (derived).
- **Base images** are resolved and pulled on the host, where "registry credentials and network access live" [ctr: Sources/ContainerBuild/BuildImageResolver.swift:26-32], and read by BuildKit through a read-only content-store proxy [shim: pkg/content/content.go:35-38].
- **The context** moves lazily: for each path BuildKit asks for, the host streams a tar [ctr: Sources/ContainerBuild/BuildFSSync.swift:180-357], and the shim applies `.dockerignore` with patternmatcher [shim: pkg/fssync/walk.go:34-47, 89-156].
- **The result** is BuildKit's `oci` exporter's tar, copied into a virtiofs share, which the host loads and tags [shim: pkg/build/build.go:73-120, 189-220; ctr: Sources/ContainerCommands/BuildCommand.swift:420-440].
- **Differences from Docker** it ships:
  - Dockerfiles of 16 KiB or more are refused [ctr: Sources/ContainerCommands/BuildCommand.swift:275-287];
  - an untagged build is named by a UUID [ctr: Sources/ContainerCommands/BuildCommand.swift:134-137];
  - a result with no filesystem at all gets a marker layer, `/.container-metadata-only`, because "OCI manifests require a layers array" [shim: pkg/build/frontend.go:388-419];
  - it uses `local.differ=none` to "ignore apple's xattrs while diffing" [shim: pkg/build/frontend.go:302-313].
- **Cross-architecture:** Rosetta through binfmt_misc [cz: Sources/Containerization/Vminitd+Rosetta.swift:20-34; Sources/ContainerizationOS/Linux/Binfmt.swift:63-70].

**BuildKit with Kata: a VM per RUN, possible upstream, never documented**

- BuildKit's containerd worker takes a runtime (`[worker.containerd.runtime] name/path/options`) [bk: docs/buildkitd.toml.md:170-174; cmd/buildkitd/main_containerd_worker.go:322-340]. On the PR that added it, a Moby contributor wrote that "users may want to e.g. run their builds with Kata, just like their regular containers" [bk4279].
- Each ExecOp makes a new containerd container and task with that runtime, and deletes both at the end [bk: executor/containerdexecutor/executor.go:223-253].
- Kata treats a lone container as a sandbox of its own, "analogous to docker run", and creates a VM for it [kata: shimv2/create.go:111-114, 147-159, 199-203]. So each RUN would boot its own VM (derived).
- With containerd's EROFS snapshotter, Kata attaches each `layer.erofs` read-only as a block device, adds an ext4 upper or a guest directory, and overlays them in the guest [kata: vc/fs_share_linux.go:594-670]. That snapshotter turns a committed directory into an EROFS blob on commit [ctrd-docs: erofs.md:318-328].
- dockerd's BuildKit cannot do this: it always uses the runc executor [moby: bn/executor_linux.go:56-60, 80-92].
- That it works end to end, and what a step costs: UNVERIFIED. Neither project documents or tests it.
- Since Linux 6.7 a binfmt_misc registration ends with its mount, which breaks registering an emulator in one container and using it from BuildKit in another [kata13517].

**Others**

- Docker Desktop's builds use the daemon's BuildKit, so runc steps inside its one VM [ddocs: B/builders/drivers/_index.md:16] (inference). Docker Build Cloud gives each builder "a single Amazon EC2 instance" [ddocs: content/manuals/build-cloud/_index.md:44-47].
- go-microvm does not build: no code reads a Dockerfile (derived by grep). It flattens pulled images into a host directory served over virtio-fs [gmv: image/pull.go:246-319; docs/ARCHITECTURE.md:102-130].
- No primary source was found for a Firecracker-based or other per-step-VM builder.

**What they reuse:** Apple reuses all of BuildKit that executes, plus its frontend, and replaces the client, the transport, the context transfer, base-image pulls and the import. Kata would reuse all of BuildKit and replace only the runtime. Nobody ships a microVM per RUN (derived).

### 2.13 shards today

- **Images** (D15, D18): blobs verified by digest; layers applied as containerd applies them, whiteouts first [shards: crates/image/src/layer.rs:1-20]; one flattened EROFS per ChainID, rebuilt whole from the layers [shards: crates/image/src/store.rs:460-491].
- **The config** is read only in part: platform, `User`, `Env`, `Entrypoint`, `Cmd`, `WorkingDir` and the DiffIDs [shards: crates/image/src/oci.rs:88-122]. A build must carry every field through (derived).
- **The guest root** is the image's EROFS from pmem with DAX, under an overlay with a tmpfs upper, `volatile` [shards: crates/init/src/run.rs:177-192], plus Docker's mounts [shards: crates/init/src/run.rs:199-229]. There is no network, `/etc/hosts` or `/etc/resolv.conf` yet [shards: docs/design/architecture.md:198, 934-936].
- **The guest kernel** has overlayfs with `redirect_dir`, `index`, `xino` and `metacopy` off by default, tmpfs xattrs, ext4, virtio-blk and binfmt_misc, and no virtio-fs [shards: resources/kernel/firecracker-aarch64-6.18.config:966, 1890, 2903, 2948-2956, 2993; resources/kernel/firecracker-x86_64-6.18.config:961, 1834, 2828, 2873-2881, 2919], plus EROFS [shards: resources/kernel/shards.config:7-11].
  - `redirect_always_follow` is on, so the default is `redirect_dir=follow`: redirects are followed but never created [linux: fs/overlayfs/params.c:14-24, 119-124; Documentation/filesystems/overlayfs.rst:239-247].
- **Devices:** virtio-pmem is read-only and 2 MiB aligned [shards: crates/vmm/src/devices/virtio/pmem.rs:1-6, 20-29]; virtio-blk is a host file, read-write or read-only, with flush but no discard [shards: crates/vmm/src/devices/virtio/block.rs:1-5, 145-154].
- **The run protocol** carries stdio and the exit status in stdcopy frames of at most 1 MiB, over vsock [shards: crates/abi/src/run.rs:1-19].
- **A run ends with its main process;** everything left is killed [shards: docs/design/architecture.md:219-220].
- **Templates** are named by the rootfs, kernel, init and VM shape [shards: docs/design/architecture.md:589-598].
- **Run latency** through the daemon (measured):
  - on the M5 Max, a booted run of `exit 0` took 34.8 ms at p50 and 36.9 ms at p99, and a pooled template run 2.97 and 3.45 ms [shards: docs/benchmarks.md:438-446];
  - on a nested-KVM GitHub runner, 245 and 257 ms, against 57.8 and 61.4 ms [shards: docs/benchmarks.md:478-487].
- **Command lines** are read as docker/cli reads them, held to its answers by an oracle that runs the CLI's own code [shards: scripts/docker-cli/generate:1-15; docs/design/architecture.md:816-838].

### 2.14 Where the reference disagrees with the code

The published reference is `ref.md` (§2.2), so each of these is visible to users.

| The reference says | The code does | Source |
|---|---|---|
| `${v#…}`, `%`, `/`, `//` are pre-release only | always available | [bk: ref.md:387-431; df/shell/lex.go:427-481] |
| nothing of `${v:?}`, `${v?}` | both work, and fail the build | [bk: df/shell/lex.go:411-426] |
| its list of instructions that expand variables | also ARG defaults, `--mount` values, FROM `--platform`, ADD `--checksum`, heredoc bodies | [bk: ref.md:453-466] vs §2.3 |
| heredocs on RUN and COPY, with "regular here-doc" rules | ADD too; COPY bodies use non-POSIX backslash rules | [bk: ref.md:3133-3136, 3201] vs §2.3 |
| ENV "always overrides" an ARG of the same name | only if the ENV comes after; an ARG after an ENV re-sets it for RUN, not in the config | [bk: ref.md:2613-2617; d2l/convert.go:1708-1713] (derived; UNVERIFIED by a build) |
| an unset `$DIRNAME` in WORKDIR prints `/path/$DIRNAME` | it expands to empty: `/path/` | [bk: ref.md:2507-2518; df/shell/lex.go:338-374] (derived) |
| eight automatic platform args | also `BUILDOSVERSION`, `TARGETOSVERSION`, `TARGETSTAGE` | [bk: ref.md:2723-2732; d2l/platform.go:45-57] |
| EXPOSE takes TCP or UDP | also sctp, ranges, and `ip:host:port` with a warning | [bk: ref.md:1231-1256; d2l/convert_expose.go:88-211] |
| STOPSIGNAL is `SIG<NAME>` or a number | also names without `SIG`, any case; 0 refused | [bk: ref.md:2918-2922] |
| a missing `--chown` user or group fails the build | 0, silently | [bk: ref.md:1917-1920; solver/llbsolver/ops/user_linux.go:47-67, 93-112] (inference) |
| without `--chown`, files are 0:0 | only context files, which clients reset; `--from` keeps owners | [bk: ref.md:1903-1907; solver/llbsolver/file/backend_unix.go:13-31] |
| several sources need a destination ending in `/` | no check; only the legacy builder enforces it | [bk: ref.md:1393-1394; moby: daemon/builder/dockerfile/copy.go:107-109] (derived by grep) |
| `--exclude` and `.dockerignore` use `filepath.Match` | patternmatcher's own regex, with `**`, `!` and parent matching | [bk: ref.md:2080-2084; ddocs: B/concepts/context.md:558-559] vs §2.8 |
| a URL's mtime is not part of the cache decision | its `Last-Modified` is part of the key | [bk: ref.md:1498-1503; source/http/source.go:243-264] |
| `--link` is described without exceptions | `--chmod` turns it off | [bk: ref.md:1924-1998; d2l/convert_copy.go:334] |
| — | `COPY --link --chown=<name>` has no stage to look names up in, and fails | [bk: ops/file.go:282-300] (inference) |

## 3. Implications for shards (ranked)

Ranked by how much each decides whether `shards build` is a drop-in: which Dockerfiles work, what image comes out, and what the user sees. The designs marked (inference) are options, not decisions.

1. **Split the work as BuildKit does: the frontend and every file operation in `shardsd`, only RUN in a microVM.**
   - *Why:* in BuildKit only RUN executes anything from the image. COPY, ADD, WORKDIR's mkdir, heredoc files, downloads and the export all run in its own process (§2.1). Apple runs the frontend in-process too [shim: pkg/build/frontend.go:115-121, 326-378], and resolves bases on the host [ctr: Sources/ContainerBuild/BuildImageResolver.swift:26-32].
   - *What* (inference): a Rust frontend in `shardsd` that parses, expands, resolves stages and computes each stage's config and history; host code for file operations; a step VM only for RUN.
   - *Not BuildKit itself in a builder VM.* Apple's route buys parity by running BuildKit and runc inside one VM (§2.12). shards' runtime is "not containerd, runc or any container runtime underneath" [shards: docs/design/architecture.md:22-26], and container-engine-internals R9 chose to reimplement the model.
   - *Cost:* shards owns a reimplementation of the frontend, whose reference parts ways with the code in the places §2.14 lists. Item 2 is how to hold it to the code.
2. **Hold the frontend to BuildKit's own code with an oracle, as shards-cmdline is held to docker/cli's.**
   - The parser, the lexer, `instructions` and `dockerfile2llb` are Go packages that run offline. This note's probes did so (probed), and Apple's shim links them [shim: go.mod:13].
   - *Shape* (inference, after [shards: scripts/docker-cli/generate:1-15]):
     - a script pins BuildKit v0.33.0 and runs a corpus of Dockerfiles through `Dockerfile2LLB`;
     - it records parse errors, lint warnings, each target's config JSON and history, each step's LLB (arguments, environment, working directory, user, mounts, network) and progress names, and the lexer's results;
     - shards' tests expect the same bytes.
   - *The corpus:* BuildKit's own Dockerfile tests (`df/dockerfile_*_test.go`), every row of §2.14, and every UNVERIFIED item (E9).
   - *Why:* D27's oracle already settled 110 command lines byte for byte this way [shards: docs/design/architecture.md:834-838].
3. **RUN: a VM per step, over the parent state read-only, with a fresh upper, and BuildKit's differ applied to that upper.**
   - *The root* (inference): D16's layout [shards: crates/init/src/run.rs:177-192], with the parent state as the lower, a new upper, and RUN's environment (§2.5):
     - hostname `buildkitsandbox`, no stdin;
     - `/etc/hosts` and `/etc/resolv.conf` bind-mounted read-only with BuildKit's contents;
     - afterwards, the stubs removed as `MountStubsCleaner` removes them [bk: executor/stubs.go:49-139].
   - *Why the upper holds the whole diff:* the guest kernels create no redirects and no metacopy files (§2.13). So a renamed lower directory is copied [linux: Documentation/filesystems/overlayfs.rst:196-214], and every copy-up is complete (derived). BuildKit's differ refuses redirects anyway [bk: util/overlay/overlay_linux.go:179-186].
   - *Overlay's own xattrs must not leak:* init mounts overlay as root without `userxattr`. So copy-up writes `trusted.overlay.origin` on directories and single-link files, and `trusted.overlay.impure` on their parents [linux: fs/overlayfs/copy_up.c:683-696, 945-1001], and tmpfs keeps `trusted.*` [linux: mm/shmem.c:4403-4419]. Keeping only `security.capability`, as the ChangeWriter does [bk: ctrd-archive/tar.go:645-652], drops them (derived).
   - *Where to diff* (inference): the differ reads the lower as well as the upper [bk: util/overlay/overlay_linux.go:203-212, 247-269]. Either:
     - the guest runs it, with both mounted, and sends the tar;
     - or the guest sends the raw upper (entries, xattrs, 0/0 devices, data), and the host runs the differ against the lower tree it already holds [shards: crates/image/src/layer.rs:1-20]. This keeps the parity-critical rules in host code, testable without a VM. The tree must then carry what the differ compares: mode, owner, rdev, `security.capability`, size, mtime to the nanosecond, and contents.
   - *The upper's storage* (inference): tmpfs is guest RAM, fixed when a template is saved (D25), and a step can write gigabytes. ext4 on a sparse per-step virtio-blk file bounds guest RAM, and since the file is deleted after the step, the device's lack of discard does not matter [shards: crates/vmm/src/devices/virtio/block.rs:145-154]. image-storage R4 weighs the same choice for runs. E2.
   - *Getting it out* (inference): through the run connection's 1 MiB frames [shards: crates/abi/src/run.rs:17-19], or written by the guest to a raw virtio-blk device that the host then reads as a file. E3.
   - *Why a VM per step, not per stage:*
     - BuildKit runs each RUN in a new container and keeps only the root's diff [bk: ops/exec.go:389-392, 535-553];
     - a VM per step gives the same, and whatever the step left running dies with the VM, as after a shards run [shards: docs/design/architecture.md:219-220] (derived);
     - one VM per stage would need init to end every process of a step and seal its upper before the next (inference).
4. **Keep each layer as an EROFS image the next step mounts; stack them during a build, and flatten by ChainID for `shards run`.**
   - *The next step's lower* (inference). Either:
     - flatten per step, as D18's writer does [shards: crates/image/src/store.rs:460-491], at a cost that grows with the image (E4);
     - or one EROFS per layer, whiteouts in overlay's own form (image-storage.md Table 3), stacked as lower layers, up to 500 [linux: fs/overlayfs/params.h:20; fs/overlayfs/params.c:337-339]. containerd's overlay snapshotter stacks committed uppers the same way [bk: V/github.com/containerd/containerd/v2/plugins/snapshots/overlay/overlay.go:552-615], and Kata over containerd's EROFS snapshotter attaches one `layer.erofs` per layer [kata: vc/fs_share_linux.go:594-670].
   - *Devices:* a stack needs a device per layer. virtio-pmem reads its region once, at probe [linux: drivers/nvdimm/virtio_pmem.c:78-88], while virtio-blk picks up capacity changes [linux: drivers/block/virtio_blk.c:912-955, 1672]. That matters for templates (item 10).
   - *The result:* the built image's flattened EROFS, named by its ChainID, is exactly what `shards run` boots (D18). Written at the end of the build, it lets the first run convert nothing (derived).
5. **RUN needs a network, and shards' guests have none yet.**
   - By default a RUN has the sandbox's network, a bridge under dockerd [moby: bn/executor_linux.go:27-31; bk: ins/commands_runnetwork.go:9-54]. Package installs need it (inference).
   - shards' guests have no network, `/etc/hosts` or `/etc/resolv.conf` [shards: docs/design/architecture.md:198, 934-936]. networking.md R1 plans the datapath.
   - Until it lands, every step behaves as `--network=none`, and a Dockerfile that fetches fails in its RUN (inference). This orders the work (item 13).
6. **COPY, ADD and WORKDIR on the host, writing the layer that BuildKit's differ would write.**
   - *No VM:* BuildKit runs them in its daemon through fsutil [bk: solver/llbsolver/file/backend.go:272-287] (derived).
   - *In memory, never on the host's filesystem:*
     - case-insensitive APFS corrupts names that differ only by case [shards: CLAUDE.md:50];
     - owners, devices and xattrs need privilege on a real filesystem (inference);
     - image-storage R1 already rules out extracting on the host.
   - *Semantics to reproduce* (§2.6): `filepath.Match` wildcards, contents of directories, named symlinks followed, parents 0755 or the `--chmod` mode, `--chown` names from the destination's `/etc/passwd` (0 when missing), mtimes kept.
   - *The layer* holds the new entries plus every ancestor, with its metadata in the merged view [bk: ctrd-archive/tar.go:654, 705-732]. A directory that gained an entry has a new mtime (inference), so its entry carries the build's time. Byte parity therefore needs `SOURCE_DATE_EPOCH` with `rewrite-timestamp` (item 7).
   - *The client's platform:*
     - a context read on Windows needs fsutil's rule: `0111` added, masked to `0755`, no owners or xattrs [bk: V/github.com/tonistiigi/fsutil/stat.go:43-50; V/github.com/tonistiigi/fsutil/stat_windows.go:11-16];
     - one read on macOS drops `com.apple.*` xattrs [bk: V/github.com/tonistiigi/fsutil/stat_unix.go:15-46].
7. **Test parity against a real BuildKit on the config, the history and the DiffIDs.**
   - *Comparable:* the config's bytes (§2.9), the history strings, the DiffIDs, and each layer's tar entries with their headers.
   - *Not comparable:* compressed digests, manifests, indexes and containerd-store IDs (§2.9).
   - *How* (inference): on Linux CI, build a corpus with pinned buildx and a BuildKit v0.33.0 builder, `SOURCE_DATE_EPOCH` set and `-o type=oci,rewrite-timestamp=true`. `--load` into Docker's default store cannot rewrite timestamps (§2.9). Build the same corpus with shards and compare.
   - *Without `rewrite-timestamp`,* RUN and COPY layers carry wall-clock and context mtimes, so only entry lists and the other header fields compare (derived).
   - E5.
8. **The cache: BuildKit's key contents, in shards' own store.**
   - *Keys* (inference, from §2.10): each step's key hashes its parent's key and its own content:
     - RUN: arguments, environment (proxies out), directory, user, hostname, mounts, network, security, platform;
     - COPY and ADD: the action, plus the content hash of the selected sources, without mtimes;
     - FROM: the manifest digest and platform.
     - Secret contents never enter a key.
   - *Store* (inference): the key maps to the layer's DiffID and its history entry. Layers and EROFS images are kept as D18 keeps them.
   - *Correctness before speed* (inference): BuildKit's link graph and its fast and slow lookups exist for speed. A chain of parent keys reproduces its hits and misses for one builder.
   - *COPY's hash comes before the step,* as BuildKit computes it before its lookup. A missing source fails there, with BuildKit's message (§2.6).
   - `--no-cache` skips lookups for RUN, COPY and ADD but still stores results; `--pull` resolves the base in the registry (§2.10).
9. **The command line: buildx's command in shards-cmdline, and a daemon that reads the context itself.**
   - `docker build` is buildx (§2.11). So the oracle of item 2 extends to buildx v0.37.1's command tree, and flag errors exit 125 (inference).
   - *First:*
     - a directory context; `-f` (with `-`), `-t`, `--build-arg`, `--target`;
     - `-q`, `--iidfile`, and `--progress` `plain`, `auto`, `quiet` and `none` with `BUILDKIT_PROGRESS`;
     - `--no-cache`, `--pull`, the hidden legacy flags; exit codes 0, 1 and 125.
   - *Then:*
     - `--platform` (the host's), `--label`, `--secret`, `--ssh`, `--network`, `--add-host`;
     - `-o`, `--load` as a no-op, `--build-context`, `--no-cache-filter`, `--shm-size`, `--ulimit`, `--metadata-file`.
   - *Later:* `--push`, cache import and export, attestations, `--call`, and the TTY display.
   - *The context* (inference): the thin client must stay small [PM M23], and the daemon serves only its own user [shards: docs/design/architecture.md:681]. It can read the context directory itself, handed over as a descriptor, as runs hand over stdio (D26), rather than receive a stream. It then resets owners to 0:0 as buildx does [buildx: build/build.go:1184-1207].
   - *The ID:* `-q` and `--iidfile` print the config digest on a graphdriver store, and the index digest on Docker's default store with provenance (§2.11). Only the config digest is reproducible. Which to print is a product decision (inference).
   - *Progress:* BuildKit's plain format (§2.11). Vertex numbers and their interleaving depend on timing, so tests compare each vertex's block, not whole transcripts (inference).
10. **The cost of starting a step: boot first, then a builder template restored per step.**
    - *Measured* (§2.13): a booted run takes 34.8 ms at p50 against 2.97 ms from a template on the Mac, and 245 ms against 57.8 ms on nested KVM.
    - *Why D25's templates do not help:* they are named by their root filesystem [shards: docs/design/architecture.md:589-598], and every step has a new one. A template per step would never be reused within a build, and across builds the cache usually answers first (derived).
    - *A builder template* (inference): saved before any root is mounted, restored per step with that step's devices attached, it would serve every step. The root would then come on virtio-blk, whose capacity changes the guest follows, or on pmem regions sized ahead (item 4). E1.
    - *First milestone:* boot per step. Against steps that run for seconds, 35 ms is small on the Mac; 245 ms on nested KVM is not (derived).
11. **Mounts: secrets and SSH over vsock, cache mounts as disks the daemon keeps.**
    - *Secrets* (inference): the bytes travel on the run connection, and init writes them to a private tmpfs at the target, mode 0400, or into the environment for `env=` (§2.5). They are never in the upper, so never in the layer.
    - *SSH* (inference): agent connections forwarded over vsock.
    - *Cache mounts* (inference): one disk image per ID, kept by the daemon and attached read-write, never part of the image. `locked` is exclusive use. `shared` between concurrent steps needs a shared filesystem, and the kernel has no virtio-fs (§2.13). E8.
    - *Bind mounts* of the context or a stage (inference): a read-only EROFS of the source.
12. **Other architectures: binfmt_misc in the guest, later.**
    - The guest's architecture is the host's [shards: CLAUDE.md:41]. BuildKit runs a foreign RUN under QEMU user-mode emulation [bk: docs/multi-platform.md:19-20; ops/exec.go:451-464], and Apple uses Rosetta through binfmt_misc (§2.12). shards' kernels have binfmt_misc (§2.13).
    - A registration now lives only as long as its binfmt_misc mount [kata13517]. So init must hold the mount for the VM's life (inference).
    - Dockerfiles that cross-compile (`FROM --platform=$BUILDPLATFORM`) run natively and need none of this (derived).
13. **Order of work.** Each stage below is testable against BuildKit on its own, and the first two need no VM (derived from §2.1).
    1. The frontend and its oracle. Metadata-only builds (FROM, ENV, LABEL, ARG, USER, EXPOSE, VOLUME, CMD, ENTRYPOINT, SHELL, STOPSIGNAL, HEALTHCHECK, ONBUILD) written to the store, with the first flags and plain progress.
    2. COPY, ADD of local files and archives, and WORKDIR, on the host; the context and `.dockerignore`; their cache keys.
    3. RUN in a booted VM without a network: the diff, per-layer EROFS images, RUN's cache keys, and parity CI (item 7).
    4. After networking: RUN's network, `/etc/hosts` and `/etc/resolv.conf`; ADD from URLs and git; secret, SSH and cache mounts.
    5. Builder templates or warm step VMs; stages built in parallel.
    6. `-o` exporters, `--push`, other platforms, attestations.

**Constraint conflicts found**

| Constraints in tension | Evidence |
|---|---|
| Parity with `docker build` vs no containers underneath | Apple gets parity by running BuildKit and runc in one VM [shim: pkg/buildkit/config.go:26-44]; shards' runtime is "not containerd, runc or any container runtime underneath" [shards: docs/design/architecture.md:22-26] |
| RUN's default network vs guests without one | [moby: bn/executor_linux.go:27-31]; [shards: docs/design/architecture.md:934-936] |
| Byte-comparable layers vs Docker's default image store | `rewrite-timestamp` conflicts with `unpack`, which moby turns on [bk: exporter/containerimage/export.go:325-331; moby: bn/exporter/wrapper.go:57-59] |
| A VM per step vs the 5 ms target | a boot costs 34.8 ms at p50 on the Mac; templates are named by their root, and every step's root is new (§2.13) |
| A tmpfs upper vs steps that write gigabytes | guest RAM is fixed when a template is saved (D25); image-storage R4 |
| A reproducible `-q` ID vs Docker's default output | the config digest vs the index digest (§2.11) |
| COPY on the host vs APFS | names that differ only by case [shards: CLAUDE.md:50] |
| Following the reference vs matching Docker | the disagreements of §2.14 |

## 4. Open questions needing our own measurement

- **E1. The cost of a step's VM.**
  - `RUN true` repeated: booted, restored from a builder template, and one VM per stage.
  - Split into VM start, root mount, command, diff and teardown.
  - macOS/HVF and Linux/KVM; n, p50, p90, p99 and max, with host, OS and revision.
- **E2. The upper.** tmpfs against ext4 on a sparse virtio-blk file, for `apk add`, `apt-get install`, `pip install`, `npm ci` and a C compile: wall time, guest RSS, host disk, and the diff's size.
- **E3. Moving a diff out.**
  - vsock frames against a raw virtio-blk device, for 1 MiB, 100 MiB and 1 GiB of changes: throughput and CPU on both sides.
  - The guest walking and writing the tar, against the raw upper with the differ on the host (§3 item 3).
- **E4. The next step's root.** One EROFS per layer, stacked, for N = 1 to 100 (mount time, `stat` and `open` latency, memory), against a flattened EROFS per step (write time against image size).
- **E5. Parity with BuildKit v0.33.0.**
  - A corpus built by both (§3 item 7): BuildKit's Dockerfile tests, official images' Dockerfiles, every row of §2.14 and every item of E9.
  - Count the configs, histories, DiffIDs and tar entries that match, and classify every difference.
- **E6. The context.** Walking, filtering and hashing a 100,000-file context in the daemon; a rebuild after one file changes; what handing the context over adds to the thin client.
- **E7. A fully cached build.** Command to last line, for 10 and 100 steps, and how much of it is content hashing.
- **E8. Cache mounts.** A disk per ID attached exclusively, against concurrent steps sharing one; `npm ci` with a warm cache.
- **E9. The unverified behaviours**, each one small build on the pinned BuildKit:
  - WORKDIR over an existing directory: an empty layer, or none;
  - an ARG declared after an ENV of the same name;
  - `--chown` with an unknown name, and `COPY --link --chown=<name>`;
  - several sources without a trailing slash;
  - an empty `<Dockerfile>.dockerignore` beside a `.dockerignore`;
  - the order of the internal vertices, and the exit status of a failed RUN (1);
  - the `-q` ID on a containerd store with default provenance.

## 5. References

**Source code** (paths and lines cited inline)

- moby/buildkit v0.33.0 (`dddd5621af04`), also tagged dockerfile/1.27.0, with its vendor tree:
  - containerd v2.3.4 (`pkg/archive`, `pkg/oci`, `plugins/snapshots/overlay`);
  - tonistiigi/fsutil `83cac42c1c52`, moby/patternmatcher v0.6.1, moby/go-archive v0.2.0;
  - moby/sys `user` and `signal`, containerd/platforms, moby/docker-image-spec, opencontainers/image-spec's Go types.
- docker/buildx v0.37.1 (`0b265a9f62db`), docker/cli v29.8.1 (`4a63305d7433`), moby/moby docker-v29.8.1 (`464cd50c3d9e`).
- apple/container 1.5.0 (`d265d669ecae`), apple/containerization 0.47.0 (`bc994b88df46`), apple/container-builder-shim 0.13.1 (`e18d2182fd06`).
- kata-containers 4.2.0 (`c7351e797eff`); go-microvm v0.0.41 (`7e148d855378`).
- Linux 6.18.48 from git.kernel.org's stable tree:
  - `Documentation/filesystems/overlayfs.rst`;
  - `fs/overlayfs/{params.c,params.h,copy_up.c}`, `mm/shmem.c`;
  - `drivers/block/virtio_blk.c`, `drivers/nvdimm/virtio_pmem.c`.
- Go 1.26.1: `src/archive/tar/common.go`, `src/encoding/json/encode.go`, from the local toolchain.

**Specifications**

- OCI image-spec v1.1.1 (`147f9c13cedb`): `config.md`, `layer.md`.

**Official documentation**

- The Dockerfile reference: BuildKit's `frontend/dockerfile/docs/reference.md` at v0.33.0, published at https://docs.docker.com/reference/dockerfile/.
- BuildKit's `docs/build-repro.md`, `docs/multi-platform.md`, `docs/buildkitd.toml.md`, `docs/dev/solver.md` and `docs/dev/merge-diff.md`.
- docker/docs at `e58e9f955d8a`: `content/manuals/build/` (context, cache invalidation, garbage collection, multi-stage builds, builders and drivers) and `content/manuals/build-cloud/_index.md`.
- containerd v2.4.1 `docs/snapshotters/erofs.md`.

**Pull requests and issues:** moby/buildkit #4279 [bk4279]; kata-containers #13517 [kata13517].

**Registry manifests** [assets], read 2026-09-29: `ghcr.io/apple/container-builder-shim/builder:0.13.1` and `moby/buildkit:v0.26.2`.

**Our notes:** container-engine-internals.md §2.6 and R9; image-storage.md R1, R4, R5 and Table 3; networking.md R1; D15, D16, D18, D25, D26, D27 (architecture.md); PM M23; docs/benchmarks.md ("Image").
