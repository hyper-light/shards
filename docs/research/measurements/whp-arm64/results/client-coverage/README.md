# Native client coverage: real-VM checks pending

The running `real-04` suite's Compose binary reported three passed cases in 0.02 seconds,
but its stderr says `SKIP: no docker compose on this host` three times. Those results
prove no Compose VM behavior. The pre-existing Buildx E2E file also compiled only on
Unix, so Windows ran none of its BuildKit-in-a-microVM case.

The candidate enables that same Buildx test on Windows. Its platform setup finds `.exe`
clients/plugins, copies the plugin into the test's isolated Docker config, and names the
test daemon's Engine pipe as the other Windows client tests do. The Dockerfile, BuildKit
version, commands, assertions and deadlines are unchanged. The launcher adds its kit's
`clients` directory to PATH and refuses qualification without the Docker CLI, Compose
and Buildx clients. Snapshot and builder-only diagnostics retain their separate scope.

The exact repository-pinned native clients are prepared on the Mac:

- [Compose v5.6.0](https://github.com/docker/compose/releases/tag/v5.6.0): upstream Windows
  arm64 asset, verified against its published `.sha256` and GitHub's asset digest.
- [Docker CLI v29.8.1](https://github.com/docker/cli/tree/4a63305d74332de5ceba7fcbccbc3cbb7412f5ba):
  exact clean commit pinned by `scripts/docker-cli/generate`, built for Windows arm64
  with upstream's build scripts, Go 1.26.8, vendored dependencies and CGO disabled.
  Upstream's Windows resource generator uses GNU sed; the measured build uses installed
  GNU sed 4.10 and upstream's pinned goversioninfo 1.5.0. The generated resource and
  version metadata do not alter Docker's source. BuildTime records the source commit's
  timestamp; no system clock or test clock was changed.
- [Buildx v0.37.1](https://github.com/docker/buildx/releases/tag/v0.37.1): upstream Windows
  arm64 asset, verified against GitHub's published asset digest. The release's
  `checksums.txt` has no Windows asset entry; it was not used as a matching checksum.

`clients.json` records URLs, sizes, versions and hashes. All three PE headers identify
ARM64 (`0xaa64`). They have not been deployed to the Windows fixture or version-checked
there yet. No Docker Desktop or Docker daemon is installed for these client tests.

All eight lint targets, a fresh arm64 guest build and all 142 Windows test executable
builds passed. Native PowerShell 5.1 parsed the candidate launcher. A second native
control invoked its preflight with an absent kit and the missing clients, and observed
`Missing qualification client: docker.exe` before manifest access or test launch. It
created no fixture path and started no VM. These controls do not prove feature behavior.

The frozen candidate contains the new Buildx test executable and manifest, the launcher,
clients and the same immutable storm-report guest. The other 141 Windows test
executables are byte-identical to `real-04`'s deployed build. `candidate.json` records
that comparison and the exact compiled source patch. The original running full suite
and both queued diagnostics retain their original binaries and guests.

Next, after the current full suite and both diagnostics have finished and their raw
results are retained, run native client version checks and the original Compose,
Buildx and Docker CLI Engine cases without client-related skips. Their selected results
still require a new complete, unfiltered real-WHP suite before Step 10 can close. The
current full suite already has native failures and cannot qualify this goal.

The serial client queue is prepared behind the existing full, snapshot and builder
tasks. All seven embedded PowerShell blocks passed the native parser; the subsequent
Python-only transport retry change retains their exact hashes. The queue checks native
task completion and archives each predecessor before deploying the clients into a
separate kit. No client deployment or VM result has occurred. An SSH transport failure
is an observation failure, and cannot certify that a native task ended.

The recorded queue harnesses serialize prerequisite observations through local launch
phases and back off for five minutes on SSH transport failure. Those gates permit an
observation; native completion guards remain authoritative. They do not change any
native test workload or deadline.

`harness/` preserves the executed local measurement layout and paths. Transcript views
normalize line endings and display padding; `transcript-format.json` records original
capture and display hashes. Clients, build outputs and original captures remain local;
no credential file or memory dump is part of this evidence.
