# Docker / Compose / Dockerfile / OCI: the external contract shards must reproduce

Research date: 2026-09-28. Method: shallow `git clone --branch <tag>` of every repo below into a scratch dir and direct reading of the pinned files. docs.docker.com pages were fetched on the research date. Line numbers refer to the pinned trees. Anything not read at source is marked **UNVERIFIED**.

**Citation keys**

| Key | Source | Pin |
|---|---|---|
| M | moby/moby | `docker-v29.8.1` (464cd50) |
| SW | M `api/swagger.yaml`, Engine API **v1.56** | same |
| CLI | docker/cli | `v29.8.1` (4a63305) |
| CO | docker/compose | `v5.5.1` (5f94fb0) |
| CG | compose-spec/compose-go | `v2.15.0` (4ddbf11) |
| CS | compose-spec/compose-spec | `main@914ec15` (2026-09-17) |
| BK | moby/buildkit | `v0.33.0` = `dockerfile/1.27.0` (dddd562) |
| DI | distribution/distribution | `v3.1.2` (3220848) |
| RS / IS / DS | opencontainers runtime-spec / image-spec / distribution-spec | `v1.3.0` / `v1.1.1` / `v1.1.1` |
| RT | opencontainers/runtime-tools | `main@8a4db57` (last release v0.9.0, 2019) |
| BX | docker/buildx: official Docker source, but not on the enumerated source list. Used for one fact, which is flagged where it appears | `v0.37.1` (0b265a9) |

## 1. Scope

**In scope.** This document covers what stock clients depend on. The clients are the docker CLI 29.8, docker compose 5.5, buildx, and SDKs. shards' own in-VM engine, CLI, compose and builder must reproduce this contract. The document also covers the OCI specs underneath, the conformance suites that prove compatibility, and how to add isolation controls without breaking stock clients.

**Out of scope.** Swarm (`/swarm`, `/services`, `/nodes`, `/tasks`, `/secrets`, `/configs`), managed Engine plugins, and Windows are out of scope. One exception: compose still calls `/info` to check Swarm state (§2.2), so shards must answer that.

**Version note.** "docker compose v2" is now **v5**. The current release is v5.5.1, module `github.com/docker/compose/v5` [CO pkg/api/labels.go:L22].

## 2. Findings

### 2.1 Docker Engine API (Q1)

**Versions**
- The daemon supports **1.56** max. The default minimum is 1.40, overridable via `DOCKER_MIN_API_VERSION`, with a hard floor of 1.24 [M daemon/config/config.go:L61-71]. SW has `basePath: /v1.56` [SW:L22-25].
- 1.56 first shipped in docker-v29.8.0; v29.7.2 was 1.55 [M@docker-v29.8.0 and @docker-v29.7.2, config.go:L64].
- **Discrepancy.** The docs version matrix says "29.8 → max 1.55" [docs: /reference/api/engine/]. Source is authoritative.
- CLI v29.8.1 vendors moby/client v0.6.0 (max 1.56, min 1.40) [CLI vendor.mod:L36-37; vendor/github.com/moby/moby/client/client.go:L112-116].
- Compose v5.5.1 uses client v0.5.1, so it tops out at **1.55** [CO go.mod:L32-33; moby `client/v0.5.1` client/client.go:L112].
- API deltas from 1.52 to 1.56 [M api/docs/CHANGELOG.md:L16-131]:
  - events content negotiation;
  - removal of the deprecated `NetworkSettings` fields;
  - `Config` fields omitted when empty;
  - `/images/{name}/attestations` (1.55);
  - `HostConfig.Umask` and the `annotation` filter (1.56).

**Routing and version negotiation**
- Every route is registered at both `/v{version:[0-9.]+}/…` and unversioned. Unknown paths return `404 {"message":"page not found"}` [M daemon/server/server.go:L23,L127-147].
- The version middleware stamps every response with three headers: `Server: Docker/<ver> (<os>)`, `Api-Version`, and `Ostype`. A version outside [min, max] returns 400 with "client version X is too new/old…" [M daemon/server/middleware/version.go:L53-86]. Unversioned paths are deprecated [SW:L49-62].
- `/_ping` answers GET and HEAD. Headers: `Api-Version`, `Builder-Version` (default `"2"` = BuildKit), `Docker-Experimental`, `Swarm` (inactive, pending, error, locked, active/worker, active/manager), `Cache-Control`, `Pragma` [SW #/paths/~1_ping, L10750-10845].
- How the Go client negotiates:
  - It sends HEAD `/_ping` (unversioned) and falls back to GET [M client/ping.go:L108-144].
  - It only ever **downgrades**: it uses min(client max, `Api-Version`). A server below 1.40 is an error. A missing header means the client uses its own max [M client/client.go:L337-368; ping.go:L100-105].
  - The header must parse as `<int>.<int>` [M client/utils.go:L50-59].
  - `DOCKER_API_VERSION` pins the version and disables negotiation [M ping.go:L21-23].
  - Negotiation is lazy (first request), and a failure is ignored silently [M client.go:L303-325].

**Errors and forward compatibility**
- The error body is `{"message": "..."}` [SW:L37-47; M server.go:L94-108].
- HTTP status comes from the error class [M daemon/server/httpstatus/status.go:L16-74]:

  | Error class | HTTP |
  |---|---|
  | NotFound | 404 |
  | InvalidArgument | 400 |
  | Conflict | 409 |
  | Unauthorized | 401 |
  | Unavailable | 503 |
  | PermissionDenied | 403 |
  | NotModified | 304 |
  | NotImplemented | 501 |
  | anything else | 500 |

- Clients map these back and branch on them. See the pull-on-404 behaviour in §2.3 and "409 = already exists" in §2.2.
- **Open schema.** The spec says: "the server will ignore any extra query parameters and request body properties", and clients must ignore extra response properties [SW:L65-70].
  - Code confirms it: requests are decoded with `json.NewDecoder` without `DisallowUnknownFields`.
  - Trailing data returns 400 "unexpected content after JSON".
  - A non-empty body must carry `Content-Type: application/json` [M daemon/server/httputils/httputils.go:L46-87; daemon/internal/runconfig/config.go:L17-97].
- `X-Registry-Auth` is base64url(JSON `{username,password,serveraddress}` or `{identitytoken}`) [SW:L72-100]. It is required on push [SW #/paths/~1images~1{name}~1push].

**Hijacked streams: attach and exec start**
- The client sends `Upgrade: tcp` and `Connection: Upgrade`.
- The current Go client **requires `101`**. Anything else fails with "unable to upgrade to tcp, received 200". It then returns a conn with `CloseWrite`, a half-close that signals stdin EOF [M client/hijack.go:L16-96].
- The server writes the response raw: `HTTP/1.1 101 UPGRADED\r\nContent-Type: <ct>\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n` [M daemon/server/router/container/container_routes.go:L1129-1193; exec.go:L71-156; SW L9243-9340].
  - `<ct>` is `application/vnd.docker.multiplexed-stream` when there is no TTY and API ≥ 1.42; otherwise it is `…raw-stream`.
  - With no `Upgrade` header, the server answers `200 OK` with raw-stream.
  - Errors after the hijack are written inline, as a raw HTTP status line for attach or as text on stdout for exec.
- **stdcopy frame format.**
  - Header: `[type,0,0,0,uint32 BE len]`.
  - Types: 0 = stdin (emitted to stdout), 1 = stdout, 2 = stderr, **3 = systemerr**, which the client turns into an error [M api/pkg/stdcopy/stdcopy.go:L14-26,L78-130].
  - With a TTY there is no framing.
- A WebSocket variant also exists: `/containers/{id}/attach/ws` [SW L9414].

**Logs, wait and lifecycle**
- **Logs** is not hijacked.
  - The server flushes `200` immediately. Content-Type is multiplexed (no TTY, ≥ 1.42) or raw.
  - Later errors are sent in-band as systemerr frames "Error grabbing logs: …".
  - `timestamps=1` prefixes each line with fixed-width RFC3339Nano and a space. `since`/`until` are unix timestamps. `tail` takes an int or `all`. At least one of stdout/stderr is required [M container_routes.go:L192-275; daemon/server/httputils/logstream/logstream.go:L24-100].
  - An experimental JSON log format exists for ≥ 1.54 (`format=json`) [M container_routes.go:L225-240].
- **Wait.**
  - Conditions are `not-running` (default), `next-exit` and `removed`.
  - Since 1.30 the headers are written and flushed immediately, so a client can register the wait before `start`.
  - The body is `{StatusCode, Error:{Message}}` [M container_routes.go:L409-470].
- **Status codes** [SW L8412-9571]:

  | Endpoint | Codes and notes |
  |---|---|
  | create | 201, 400, 404 (no such image), 409, plus `Warnings`. Name must match `^/?[a-zA-Z0-9][a-zA-Z0-9_.-]+$`. Takes a `platform` query. |
  | start | 204, 304 |
  | stop | 204, 304 (`signal`, `t`) |
  | kill | 409 if not running |
  | rename | 409 if the name is in use |
  | pause, unpause, restart | 204 |
  | update | 200 |
  | delete | 409 if running unless `force` (also `v`, `link`) |
  | exec create | 201 `{Id}`, 409 if paused. ConsoleSize is honoured only on ≥ 1.42 and with a TTY [M exec.go:L90-103]. |

- **Archive** [SW L9575-9721]:
  - HEAD returns the `X-Docker-Container-Path-Stat` header (base64 JSON).
  - GET returns a tar.
  - PUT takes a tar (identity, gzip, bzip2 or xz), with `noOverwriteDirNonDir` and `copyUIDGID`.
- **Stats**: `stream` (default true), `one-shot`, the CPU%/memory% formulas and cgroup v1/v2 fields [SW L8835-8904].

**Streaming JSON: pull, push, build, load**
- The stream is NDJSON; each object ends with `\r\n`.
- Fields: `stream`, `status`, `progressDetail{current,total,start,hidecounts,units}`, `id`, `errorDetail{code,message}`, `aux`. Errors also carry a legacy `"error"` string.
- Once output has been flushed, the status stays 200 and errors are in-band [M api/types/jsonstream/message.go:L8-15; progress.go:L4-10; daemon/internal/streamformatter/streamformatter.go:L15-43; daemon/server/router/image/image_routes.go:L159-162].
- `/images/create` with an empty `tag` pulls **all** tags [SW L10092-10186].
- `/build` takes a tar context plus query params: `dockerfile`, `t` (repeatable), `buildargs`/`labels` (JSON), `target`, `platform`, `networkmode`, `cachefrom`, `pull`, `nocache`, `outputs`, and `version=1|2` [SW L9819-10027; M build_routes.go:L150-168].
- BuildKit builds emit `aux` messages with id `moby.buildkit.trace` (protobuf status) and `moby.image.id` [M daemon/internal/builder-next/builder.go:L461-480].

**BuildKit transport**
- `/session` and `/grpc` are HTTP/1.1 `Upgrade: h2c` hijacks. Both are deprecated since 1.53 but still served [SW #/paths/~1session; M daemon/server/router/grpc/grpc.go:L1-7,L65; grpc_routes.go:L14-35].
- The daemon also serves native HTTP/2 gRPC on the API socket. It routes on `Content-Type: application/grpc` [M daemon/command/httphandler.go:L38-48].
- Stock buildx's `docker` driver still dials `/grpc` (h2c) and `/session` [BX driver/docker/driver.go:L63-71].
- The required gRPC service is `moby.buildkit.v1.Control`: DiskUsage, Prune, Solve, Status, Session, ListWorkers, Info, ListenBuildHistory, UpdateBuildHistory [BK api/services/control/control.proto:L14-25].

**Events**
- Filters and content negotiation over `application/jsonl`, `x-ndjson` and `json-seq` [SW L10906-10981].
- A container actor's `Attributes` hold the container **labels** plus `image` and `name` (no leading `/`) [M daemon/events.go:L26-38].
- `die` carries `exitCode` [M daemon/monitor.go:L108-130].

**GPUs and devices**
- `DeviceRequest` has `Driver`, `Count` (-1 = all), `DeviceIDs`, `Capabilities` (an OR of AND-lists) and `Options` [SW #/definitions/DeviceRequest L337].
- How the daemon picks a driver [M daemon/devices.go:L53-85; errors.go:L82]:
  - An empty `Driver` means the first registered driver whose capset matches.
  - A named `Driver` is used directly.
  - Otherwise it fails with `could not select device driver %q with capabilities: %v`.
- Built-in drivers: `nvidia` {gpu, nvidia, …}, `amd` {gpu, amd}, `cdi` [M devices_nvidia_linux.go:L69-73; devices_amd_linux.go:L74-76; cdi.go:L35].
- `/info` exposes `Runtimes`, `DefaultRuntime`, `DiscoveredDevices` and `CDISpecDirs` [SW #/definitions/SystemInfo].

**Health checks (run by the engine)**
- Defaults: interval 30s, timeout 30s, start_period 0, start_interval 5s, retries 3. Probe output is capped at 4096 bytes.
- The first probe runs after `interval`, or after `start_interval` while in the start period and still "starting" [M daemon/health.go:L21-41,L256-287].
- `Test` forms: `[]` inherits, `["NONE"]`, `["CMD",…]`, `["CMD-SHELL",cmd]` [SW #/definitions/HealthConfig].

**Images**
- `ubuntu` normalises to `docker.io/library/ubuntu:latest` [M vendor/github.com/distribution/reference/normalize.go:L20-39].
- Docker Hub traffic goes to `registry-1.docker.io`; the auth key is `https://index.docker.io/v1/` [M daemon/pkg/registry/config.go:L40-55].
- Schema1 support is removed [M daemon/internal/distribution/errors.go:L231].
- The **containerd image store is the Linux default** unless graphdriver data already exists [M daemon/image_store_choice.go:L105-133].

### 2.2 What docker compose v5.5.1 actually calls (Q2)

**Dependencies.** compose-go v2.15.0, docker/cli v29.7.2 (its run, exec, start and stats UX is reused), moby api v1.55.0 and client v0.5.1 [CO go.mod:L11-33].

**Endpoints compose calls directly** (the `apiClient().X` call sites in CO pkg/, excluding mocks and dryrun):
- Containers: Create, Start, Stop, Kill, Restart, Remove, Rename, Pause, Unpause, Inspect, List, Logs, Attach, Wait, Top, Export, Commit.
- Exec: Create, Start, Attach, Inspect.
- Archive: CopyTo, CopyFrom, StatPath.
- Images: Inspect, List, Pull, Push, Remove, Build.
- Networks: Create, List, Inspect, Connect, Disconnect, Remove.
- Volumes: Create, List, Inspect, Remove.
- System: Events, Info, Ping.

`compose run`, `exec` and `stats` delegate to docker/cli's `RunStart`, `RunExec` and `RunStats` [CO run.go:L71; exec.go:L30-58; cmd/compose/stats.go:L76].

**Labels** (exact keys, all prefixed `com.docker.compose.`): project, service, config-hash, container-number, volume, network, project.working_dir, project.config_files, project.environment_file, oneoff (`True`/`False`), slug, image (digest), image-volume-digest, depends_on (`svc:cond:restart,…`), version, image.builder, replace, engine, hook [CO pkg/api/labels.go:L25-70]. A user label using the `com.docker.compose` prefix is a runtime error [CS 05-services.md:L1141-1147].

**Names**

| Object | Name | Source |
|---|---|---|
| Service containers | `<project>-<service>-<n>`; the separator becomes `_` under `--compatibility` | CO service_containers.go:L127-137; api.go:L797-804; loader.go:L52-54 |
| One-off containers | `<project>-<service>-run-<12hex>`, plus labels `oneoff=True` and `slug` | CO run.go:L150-160 |
| Networks, volumes, configs, secrets | `<project>_<key>` unless `name:` is set | CG loader/normalize.go:L277 |

**Recreate logic (config hash)**
- `ServiceHash` = sha256 hex of Go `json.Marshal(ServiceConfig)`, after clearing Build, PullPolicy, Scale, Deploy.Replicas, DependsOn and Profiles [CO hash.go:L26-46].
- `Extensions` (the `x-*` keys) and `CustomLabels` are tagged `json:"-"`, so they never affect the hash [CG types/types.go:L96,L145].
- A container is recreated if any of these hold [CO reconcile.go:L761-788]:
  - policy is `force`;
  - a namespace or `volumes_from` parent was recreated;
  - the config-hash differs;
  - the image digest label differs;
  - the image-volume digest differs;
  - a running container is missing an expected network;
  - an expected named volume mount is missing.
- Recreation runs in this order: create `<oldID[:12]>_<name>`, stop the old container, remove it, **rename** the new one [CO reconcile.go:L880-930].
- Networks and volumes are hashed too. A diverged volume prompts the user before it is recreated [CO reconcile.go:L203-224,L353-398].

**Create mapping** [CO create.go:L257-446]
- `Config` fields: Hostname, Domainname, User, ExposedPorts, Tty, OpenStdin, StdinOnce, Attach*, Cmd, Entrypoint, Image, WorkingDir, NetworkDisabled, Labels, StopSignal, Env (config.json proxies overridden by service env), Healthcheck, StopTimeout.
- `HostConfig` fields: AutoRemove, **Annotations**, Binds, Mounts, Cap*, NetworkMode, Init, Ipc/Cgroupns/Uts/Pid/Userns modes, ReadonlyRootfs, RestartPolicy, ShmSize, Sysctls, PortBindings, Resources, VolumesFrom, DNS*, ExtraHosts, SecurityOpt, StorageOpt, Privileged, Tmpfs, **Runtime**, LogConfig, GroupAdd, Links, OomScoreAdj.
- `DeviceRequests` are built from three sources [CO create.go:L732-844]:
  - `gpus`, with capability `gpu` appended;
  - CDI-qualified `devices`, sent with `Driver:"cdi"`;
  - `deploy.resources.reservations.devices`.

**Networks**
- `NetworkMode` is set to the primary network's engine name.
- On API ≥ 1.44 all networks go into `EndpointsConfig` at create time. Older engines get `NetworkConnect` calls after create [CO create.go:L618-686; service_containers.go:L392-411].
- Aliases are the container name, the service name, and any user aliases [CO create.go:L470-479].
- `NetworkCreate` sends {Labels + hash, Driver, Options, Internal, Attachable, IPAM, EnableIPv4/IPv6}. A **409 is treated as success** [CO create.go:L1430-1480].

**depends_on**
- `service_started` is satisfied purely by start ordering.
- `service_healthy`, `service_completed_successfully` and the internal `running_or_healthy` **poll `ContainerInspect` every 500 ms** [CO service_containers.go:L159-252].
- `State.Health.Status` decides the result: `healthy` continues, `unhealthy` is an error, `starting` keeps waiting.
- No healthcheck (nil or `["NONE"]`) is an error for `service_healthy`, while `running_or_healthy` falls back to `State.Status == running`. An exited dependency is an error for both.
- `service_completed_successfully` only waits for `State.Status == exited`: exit 0 succeeds, anything else is an error.
- The code takes `Name[1:]`, so the leading `/` in inspect output is required [CO service_containers.go:L511-557].
- `required: false` only warns [CO same file:L235-298].

**Events, attach and logs**
- The monitor subscribes with `type=container`, `label=com.docker.compose.project=<p>` and `label=com.docker.compose.oneoff=False`. It handles create, start, restart and die. It reads `Actor.Attributes[name, com.docker.compose.service, exitCode]` and inspects the container on die [CO monitor.go:L125-304; filters.go:L33-51].
- **`up` attaches** (stream=1, stdout+stderr, logs=0) **before it starts containers** [CO up.go:L167-186; attach.go:L133-156]. If attach fails it falls back to logs follow.
- Output is demuxed with stdcopy unless `Config.Tty` is set [CO logs.go:L140-161].

**Build**
- BuildKit is considered enabled via `DOCKER_BUILDKIT`, `aliases.builder` in config.json, ping `Builder-Version: 2`, or simply a non-Windows `Ostype` [CLI cli/command/cli.go:L151-177].
- If BuildKit is enabled and the `buildx` plugin is present, compose execs `docker-buildx bake --file - --progress rawjson --metadata-file <tmp> [--allow fs.read=…]` with the bake JSON on stdin [CO build_bake.go:L55-74,L170-200,L392-411].
- Otherwise it uses the classic `POST /build` (version=1). That path refuses multi-arch, privileged, additional contexts, ssh and secrets [CO build_classic.go:L199,L235-247,L344-347].

**run, configs/secrets, use_api_socket, watch and swarm check**
- `run` starts dependencies, waits on them, creates the one-off container (`AutoRemove` for `--rm`), then calls `RunStart` [CO run.go:L45-200].
- Configs and secrets:
  - `file:` sources become read-only binds [CO create.go:L1204-1250];
  - `content:`/`environment:` sources are written with **PUT archive before start**, default mode 0444 [CO secrets.go:L38-170].
- `use_api_socket` binds `/var/run/docker.sock` and injects credentials at `/run/secrets/docker/config.json` [CO apiSocket.go:L27-85].
- `watch` syncs files via PUT archive tar, deletes via exec `rm -rf <paths>`, and handles rebuild with build + recreate [CO watch.go:L132,L489,L552; internal/sync/tar.go:L87].
- The Swarm check uses `/info` `Swarm.LocalNodeState` [CO compose.go:L486-499].
- API-gated features: 1.44 for multi-endpoint create, 1.48 for image mounts, 1.49 for `interface_name` [CO api_versions.go:L21-50].

**Project name**
- Precedence: `-p`, then `COMPOSE_PROJECT_NAME`, then `name:`, then the directory basename [CO docs/reference/compose.md:L181-195].
- Normalisation lowercases, keeps only `[a-z0-9_-]`, and trims leading `_`/`-` [CG loader/loader.go:L693-772].

### 2.3 Docker CLI contract (Q3)

**Command surface.** docker/cli has 181 reference pages under `docs/reference/commandline/`, the source for docs.docker.com/reference/cli/docker/. The `run` flag set was confirmed on docs, including `--annotation`, `--gpus`, `--runtime`, `--use-api-socket` and `--umask` [docs: /reference/cli/docker/container/run/].

**`docker run` flow**
1. Create. On NotFound, with the default `--pull=missing`, it prints "Unable to find image … locally", pulls, and retries [CLI cli/command/container/create.go:L338-346].
2. Attach (hijack).
3. Wait with `next-exit`, or `removed` for `--rm`. The wait is registered **before start**.
4. Start.
5. TTY resize [CLI run.go:L151-263; utils.go:L12-50].

**Exit codes** [CLI docs/reference/run.md:L275-325; docs: /engine/containers/run/]:
- 125: daemon or CLI error.
- 126: the command cannot be invoked.
- 127: the command was not found.
- Anything else is the container's exit code.

126 and 127 are derived from **substrings of the daemon's start error** [CLI run.go:L322-356]:
- "executable file not found", "no such file or directory" or "system cannot find the file specified" → 127.
- EACCES "permission denied" or EISDIR "is a directory" → 126.
- Anything else → 125. Wait errors also give 125 [CLI utils.go:L30-46].

**Output formatting**
- `--format` is Go `text/template` with the functions json, split, join, title, lower, upper, pad and truncate [CLI templates/templates.go:L18-27].
- The keywords `table…`, `json` and `raw` are handled specially [CLI cli/command/formatter/formatter.go:L19-62].
- Tables use tabwriter(10, 1, 3, ' ', 0) [same file:L104].
- Default `ps` format: `table {{.ID}}\t{{.Image}}\t{{.Command}}\t{{.RunningFor}}\t{{.Status}}\t{{.Ports}}\t{{.Names}}` [CLI formatter/container.go:L23].

**Environment variables**
- Documented: DOCKER_API_VERSION, DOCKER_CERT_PATH, DOCKER_CONFIG, DOCKER_CONTEXT, DOCKER_CUSTOM_HEADERS, DOCKER_DEFAULT_PLATFORM, DOCKER_HIDE_LEGACY_COMMANDS, DOCKER_HOST, DOCKER_TLS, DOCKER_TLS_VERIFY, BUILDKIT_PROGRESS, NO_COLOR [CLI docs/reference/commandline/docker.md:L115-133].
- Also read by code: DOCKER_AUTH_CONFIG [CLI cli/config/configfile/file.go:L88-103] and DOCKER_BUILDKIT [CLI cli/command/cli.go:L151-175].
- Actual precedence in code: `--context` > `-H` > `DOCKER_HOST` > `DOCKER_CONTEXT` > config `currentContext` [CLI cli/command/cli.go:L443-461]. The docs say DOCKER_CONTEXT overrides DOCKER_HOST [docker.md:L124], which contradicts the code. Implement the code's order, since that is what users observe.
- The default host is `unix:///var/run/docker.sock` [M client/client_unix.go:L13]. An `ssh://` host runs `docker system dial-stdio` on the remote side [CLI cli/connhelper/connhelper.go:L55-58].

**config.json**
- Keys: auths, HttpHeaders, the `*Format` keys, detachKeys, credsStore, credHelpers, pruneFilters, proxies, currentContext, cliPluginsExtraDirs, plugins, aliases, features [CLI file.go:L53-77].
- Credential helper protocol: `docker-credential-<x> store|get|erase`, JSON `{ServerURL,Username,Secret}`; identity tokens use Username `<token>` [CLI docs/reference/commandline/login.md:L97-145].
- `docker build` is forwarded to the `buildx` plugin (or to `aliases.builder`) unless `DOCKER_BUILDKIT=0` [CLI cmd/docker/builder.go:L50-130].

### 2.4 Dockerfile (Q4) [BK frontend/dockerfile/docs/reference.md, same content as docs: /reference/dockerfile/]

**Instructions.** FROM, RUN, CMD, LABEL, MAINTAINER (deprecated), EXPOSE, ENV, ADD, COPY, ENTRYPOINT, VOLUME, USER, WORKDIR, ARG, ONBUILD, STOPSIGNAL, HEALTHCHECK, SHELL.

**Parser directives: `syntax`, `escape`, `check`** (L113-360)
- They are only recognised before the first comment, blank line or instruction. Each may appear once. Keys are case-insensitive. Line continuation is not allowed, and unknown directives are treated as comments.
- `# check=skip=<Names|all>;error=true` configures build checks.
- `# syntax=` makes BuildKit **pull a frontend image** before the build (L195-215). `BUILDKIT_SYNTAX=dockerfile.v0` forces the built-in frontend instead (L2746-2759).

**Command forms**
- Exec form is a JSON array. Invalid JSON silently becomes shell form, and exec form does no variable expansion (L509-551).
- Shell form uses `SHELL` (default `["/bin/sh","-c"]`) and supports continuation with the escape char (L552-594, L3007-3030).
- Heredocs are supported for RUN and COPY (L3131-3245).
- The CMD×ENTRYPOINT table is at L2367-2393.

**Variable expansion**
- The docs cover `$v`, `${v}`, `${v:-w}`, `${v-w}`, `${v:+w}` and `${v+w}`, and label `#`, `##`, `%`, `%%`, `/`, `//` as pre-release (L361-438).
- The **lexer at v0.33.0 implements** `-`, `+` and `?` with and without `:`, plus `#`, `##`, `%`, `%%`, `/` and `//`. It rejects `:#` and `:%` [BK frontend/dockerfile/shell/lex.go:L375-470].
- Expansion applies in ADD, COPY, ENV, EXPOSE, FROM, LABEL, STOPSIGNAL, USER, VOLUME, WORKDIR and ONBUILD.
- Values are snapshotted per instruction: `ENV abc=bye def=$abc` gives `def=hello` (L456-486).

**ARG** (L2525-2836)
- An ARG is in scope from the line that declares it. ARGs declared before FROM are global.
- Proxy ARGs (`HTTP_PROXY`, `http_proxy`, …) are predefined and stay out of history and the cache key unless declared.
- `TARGET{PLATFORM,OS,ARCH,VARIANT}` and `BUILD{…}` are global-only: they must be re-declared inside a stage to use them.
- Built-ins include `BUILDKIT_*` and `SOURCE_DATE_EPOCH`.
- A changed ARG causes a cache miss at its **first use**, and RUN implicitly uses every ARG.

**COPY and ADD** (L1353-2105)

| Instruction | Options (minimum `syntax` version) |
|---|---|
| COPY | `--from`, `--chmod` (1.2), `--chown`, `--link` (1.4), `--parents` (1.20), `--exclude` (1.19) |
| ADD | `--keep-git-dir` (1.1), `--checksum` (1.6), `--chmod`, `--chown`, `--link`, `--unpack` (1.17), `--exclude` (1.19) |

ADD sources and behaviour:
- Local tar archives are **auto-extracted by content sniffing** (gzip, bzip2, xz, zstd or uncompressed) and merged like `tar -x`.
- URL sources get mode 0600 and mtime from `Last-Modified`; mtime never enters the cache key. A remote tar is not extracted without `--unpack`.
- Git sources are supported, with `HTTP_AUTH_*_<host>` secrets for authentication.
- `--link` copies into an empty directory and layer-merges the result.

**RUN flags** (L731-1107)
- `--mount` types and their options:

  | Type | Options and defaults |
  |---|---|
  | bind | the default type; read-only; `from`, `source`, `target`; `rw` writes are discarded |
  | cache | `id`, `sharing` (shared, private or locked), `mode` 0755, `uid`/`gid` 0, `from`, `source`, `ro` |
  | tmpfs | `size` |
  | secret | `id`, target defaults to `/run/secrets/<id>`, `env` (1.10), `required`, mode 0400 |
  | ssh | `id` (default `default`), target `/run/buildkit/ssh_agent.N`, `required`, mode 0600 |

- `--network`: `default`, `none`, or `host`. `host` needs the `network.host` entitlement.
- `--security=insecure` needs the `security.insecure` entitlement.
- `--device` (CDI) needs BuildKit ≥ 0.20 and the `device` entitlement.
- Minimum syntax versions for RUN options: `--device` 1.27, `--mount` 1.2, `--network` 1.3, `--security` 1.20 (L710-717).

**Other instructions**
- HEALTHCHECK defaults (L2932-3006):
  - interval 30s, timeout 30s, start-period 0s, start-interval 5s, retries 3;
  - exit 0 = healthy, 1 = unhealthy, 2 is reserved;
  - if several are given, the last one wins.
- STOPSIGNAL takes a name or a number; the default is SIGTERM (L2912-2931).
- VOLUME: changes made to the path after it is declared are discarded (L2394-2447).
- ONBUILD: `ONBUILD ONBUILD` is not allowed, and ONBUILD cannot trigger FROM or MAINTAINER. Since 1.11 it may use `COPY --from` and `RUN --mount=from=` (L2837-2911).

**.dockerignore** [docker/docs@f22c0e6 content/manuals/build/concepts/context.md:L467-617]
- The file lives at the context root. A `<Dockerfile>.dockerignore` next to the Dockerfile takes precedence.
- Patterns are preprocessed with `filepath.Clean` and matched with `filepath.Match`. `**` is supported. `!` makes an exception, and the last matching line wins.
- Leading and trailing `/` are ignored, `#` is a comment only in column 1, and the pattern `.` is ignored.

**Cache keys**
- Each operation's `CacheMap` is a base digest plus, per input, a selector and an optional content-based digest. Roots use secure stable digests: the manifest digest for images, the commit for git [BK docs/dev/solver.md:L100-170].
- The file content checksum uses v1 tarsum headers: name, mode, uid, gid, size, typeflag, linkname, uname, gname, devmajor and devminor, plus xattrs except `security.*` (but `security.capability` is kept) and `system.*`. **mtime is excluded** [BK cache/contenthash/tarsum.go:L21-75].

### 2.5 Compose Specification (Q5) [CS]

**Files and names**
- Default files: `compose.yaml` (preferred), `compose.yml`, `docker-compose.yaml`, `docker-compose.yml` [03-compose-file.md:L11-13].
- Top-level `version` is obsolete and only warns [04-version-and-name.md:L3-11].
- `name` sets the project name and is exposed as `COMPOSE_PROJECT_NAME` [L13-29]. The allowed charset is in [02-model.md:L25].

**Strictness**
- Only `x-*` keys are silently ignored [11-extension.md:L5-9].
- In compose-go the top level and `$defs.service` (93 properties) both set `additionalProperties:false` plus `patternProperties:{"^x-":{}}`. The schema has 45 `^x-` patternProperties in total, covering most nested objects [CG schema/compose-spec.json:L94-95].
- Each file is validated **before** merge [CG loader/loader.go:L515-535].

**Interpolation**
- Supported: `$V`, `${V}`, `:-`, `-`, `:?`, `?`, nesting, and `$$` as an escape.
- An unset variable warns and becomes an empty string. Interpolation applies to values only, per file, before merge [12-interpolation.md].
- compose-go also implements `:+` and `+` [CG template/template.go:L228-238]. The env_file format is in [05-services.md:L575-668].

**Merge** [13-merge.md]
- Mappings merge; sequences append.
- `command`, `entrypoint` and `healthcheck.test` replace instead of appending.
- Unique keys: volumes, secrets and configs by `target`; ports by {ip, target, published, protocol}.
- The `!reset` and `!override` tags are supported.

**include, extends, profiles**
- `include` gives each included file its own project directory. Conflicts only warn, and include is recursive [14-include.md].
- `extends` is in [05-services.md:L719-910].
- Profiles [15-profiles.md]:
  - services without `profiles` are always on;
  - targeting a service enables its profiles;
  - a reference to a disabled service is an error, not an auto-enable.

**Service attributes that matter here**
- `depends_on` long form: `condition`, `restart`, `required` [05-services.md:L369-457].
- `healthcheck`: a string test means CMD-SHELL; `disable: true`; `start_interval` [L1015-1060].
- `gpus`: a list, or `all` [L973-994].
- `deploy.resources.reservations.devices` [deploy.md:L139-221]:
  - `capabilities` is required; `gpu` and `tpu` are generic, anything else must be prefixed with the driver name;
  - `count` (`all` or an int) and `device_ids` are mutually exclusive;
  - driver-specific `options`.
- `build` attributes include `additional_contexts`, `entitlements`, `secrets`, `ssh`, `cache_to`/`cache_from`, `platforms`, `provenance` and `sbom` [build.md:L64-644].
- `develop.watch` actions are sync, rebuild, restart, sync+restart and sync+exec [develop.md:L41-141].
- Also current: `annotations`, `use_api_socket`, the post_start/pre_start/pre_stop hooks, `models` and `provider`.

### 2.6 OCI (Q6)

**Runtime spec v1.3.0**
- Runtimes **must ignore unknown properties** (they may log them) [RS config.md:L752-755].
- `annotations` is a string map with reverse-DNS keys; `org.opencontainers.*` is reserved [L716-751].
- The hook timing table (createRuntime, createContainer, startContainer, poststart, poststop; prestart is deprecated) is at [L548-664].
- New `vm` object [config.md:L527; config-vm.md]:
  - `hypervisor{path,parameters}`;
  - `kernel{path,parameters,initrd}` (required);
  - `image{path,format raw|qcow2|vdi|vmdk|vhd}`;
  - `hwConfig{vcpus,memory,deviceTree,…}`.

**Image config → runtime config** [IS conversion.md:L22-128]
- `WorkingDir` → `cwd`. `Env` → `env`; a converter may add variables but should not override the image's.
- `Entrypoint` + `Cmd` are **appended** into `args`.
- `os`, `arch`, `variant`, `os.version`, `os.features`, `author`, `created`, `Labels` and `StopSignal` become annotations. Labels win on conflict, and nothing is pulled from manifests.
- A numeric `User` is copied; otherwise it is resolved via `/etc/passwd` and `/etc/group`. An **unknown user must error**.
- `ExposedPorts` → `org.opencontainers.image.exposedPorts`. `Volumes` → mounts (SHOULD).

**Image index**
- Platform fields: architecture, os, os.version, os.features, variant. An unknown mediaType must not error, and the first match SHOULD win. The variant table is in [IS image-index.md:L30-118].
- BuildKit stores attestation manifests as index entries with `platform: unknown/unknown` and `vnd.docker.reference.type=attestation-manifest`. Platform selection must skip them [BK docs/attestations/attestation-storage.md:L95-160].

**Media types.** OCI and Docker schema2 equivalents: `manifest.list.v2`, `manifest.v2`, `container.image.v1`, `rootfs.diff.tar.gzip` [IS media-types.md:L5-70; DI docs/content/spec/manifest-v2-2.md:L27-32].

**Distribution spec v1.1.1**
- Endpoints end-1 … end-13 [DS spec.md:L765-784].
- Chunked upload [L322-428]:
  - POST with `Content-Length: 0`;
  - PATCH with an inclusive `Content-Range` matching `^[0-9]+-[0-9]+$`, in order, answered with 202 plus `Location` and `Range`;
  - out-of-order chunks get **416**; GET on the upload returns 204;
  - the closing PUT carries `?digest=<whole blob>` and returns 201; `OCI-Chunk-Min-Length` advertises a minimum chunk size.
- Cross-repo mount: 201 on success, or a **202** fallback that starts an ordinary upload [L429-453].
- Referrers [L567-739]:
  - a supporting registry never answers 404;
  - the response is an image index;
  - the `artifactType` filter is reported via `OCI-Filters-Applied`;
  - the fallback tag is `<alg>-<hex>`.

**Token auth** [DI docs/content/spec/auth/token.md:L46-248]
- A 401 carries `WWW-Authenticate: Bearer realm,service,scope=repository:<n>:pull,push`.
- The client calls GET realm with `service`, `scope` (repeatable), `offline_token` and `client_id`.
- The response has `token` and/or `access_token`, `expires_in` (**default 60 s**), `issued_at`, and `refresh_token` (only when `offline_token` was requested).
- The OAuth2 POST form uses `grant_type=password|refresh_token` [oauth.md].

### 2.7 Conformance suites (Q7)

| Suite | Point it at shards | Prerequisites | Gotchas |
|---|---|---|---|
| moby `integration/` (559 `Test*`, Go, black-box API) | `DOCKER_HOST=… DOCKER_REMOTE_DAEMON=1 go test ./integration/<pkg>`. `TestMain` builds the client with `client.FromEnv` and negotiates [M internal/testutil/environment/environment.go:L38-70]. | Frozen images `busybox:latest`, `busybox:glibc`, `hello-world:frozen`, `debian:trixie-slim`, `hello-world:{amd64,arm64}` [protect.go:L15-21]. They load from `/docker-frozen-images`, otherwise they are **pulled** [fixtures/load/frozen.go:L22-80]. Make targets: `TEST_INTEGRATION_DIR`, `TEST_FILTER`, `TESTFLAGS` [M TESTING.md:L98-128]. | `Clean()` deletes every non-protected container, image, volume and network [clean.go:L19-165], so use a dedicated engine. There are 132 `skip.If(t, testEnv.IsRemoteDaemon…)` guards, and 54 files reference `IsRemoteDaemon`. These are tests that need a local daemon, such as ones that spawn their own `dockerd` or touch the host. Some other tests assume Docker internals. |
| moby `integration-cli/` (1354 tests; deprecated) | Needs a docker binary via `TEST_CLIENT_BINARY` [integration-cli/environment/environment.go:L12]. | A real CLI | Deprecated [TESTING.md:L20]. Heavily tied to Docker internals. Use selectively. |
| docker-py integration | `hack/make/test-docker-py`. `unix` or `tcp` DOCKER_HOST only; pins docker-py `f387c3f` and deselects 4 tests, plus `test_build_squash` when not on graphdriver [M hack/make/test-docker-py:L18-34]. | Python image built from docker-py | An independent (non-Go) client, so it catches Go-client-only assumptions in the hijack/stdcopy code. docker-py content itself is **UNVERIFIED** (not read). |
| docker/cli `e2e/` | `scripts/test/e2e/run test <ENGINE_HOST>` exports `TEST_DOCKER_HOST`, plus optional `TEST_REMOTE_DAEMON` and `TEST_SKIP_PLUGIN_TESTS`. PATH starts with `./build/`, so our own CLI can be tested by dropping it there [CLI scripts/test/e2e/run:L107-122; internal/test/environment/testenv.go:L17-58]. | Registries `registry:5000` (insecure), `privateregistry:5001` (htpasswd), `tlsregistry`; engine runs with `--insecure-registry` and `--experimental` [CLI e2e/compose-env.yaml] | Hostnames must resolve from both the engine and the test runner. Tests use `registry:5000/<img>` fixtures [CLI TESTING.md:L78-86]. |
| docker/compose `pkg/e2e` | `make e2e-compose` (plugin mode) or `e2e-compose-standalone`. **DOCKER_HOST is not passed through; only `DOCKER_CONTEXT` is**, and `~/.docker/contexts` is copied in [CO pkg/e2e/framework.go:L179-188,L266-287]. `COMPOSE_E2E_BIN_PATH` selects the compose binary under test [L204-220]. | docker CLI, buildx plugin, network access (Docker Hub) | `copyLocalConfig` calls `CopyFile` only when `~/.docker/config.json` **is missing**, then asserts on open, so create that file first [L125-135]. Use a context such as `docker context create shards --docker host=unix://…`. |
| BuildKit Dockerfile tests (33 `dockerfile_*_test.go`) | The `dockerd` worker **spawns** a `dockerd` from PATH with `--config-file`, `--data-root`, `--exec-root`, `--pidfile`, `--host` and `--containerd-namespace`, then proxies gRPC via `DialHijack("/grpc","h2c")` [BK util/testutil/workers/dockerd.go:L150-240; util/testutil/dockerd/daemon.go:L29,L104-144]. Extra flags go in `BUILDKIT_INTEGRATION_DOCKERD_FLAGS`. | A dockerd-flag-compatible shim binary plus BuildKit Control gRPC | Tests Control-API parity, not just Dockerfile semantics. |
| Engine-free golden tests | `frontend/dockerfile/parser/testfiles/*/{Dockerfile,result}` (33 cases); `frontend/dockerfile/shell/envVarTest` (238 lines) and `wordsTest` [BK] | None | Cheapest parser and lexer conformance check. Run in CI first. |
| compose model differential | Compare `docker compose config` from stock against ours on a corpus. compose-go `loader/testdata` covers edge cases [CG loader/] | Stock compose binary | Canonical YAML output is part of the contract. |
| OCI distribution conformance | `go test -c` in `conformance/`, then set `OCI_ROOT_URL`, `OCI_NAMESPACE`, `OCI_CROSSMOUNT_NAMESPACE`, `OCI_TEST_{PULL,PUSH,CONTENT_DISCOVERY,CONTENT_MANAGEMENT}` [DS conformance/README.md:L1-40] | Only if shards ships a registry or cache | It tests registries, not clients. For client pull/push use DI's registry together with moby image tests. |
| runtime-tools validation | `make runtimetest validation-executables; sudo make RUNTIME=<cli> localvalidation` [RT README.md:L41-117] | node-tap or `prove`, **root**, and an OCI-CLI runtime (create/start/state/kill/delete) | Only for an OCI-runtime shim. It needs root, which conflicts with rootless, so run it inside a test VM. `main` tracks runtime-spec v1.3.0 [RT go.mod:L13], but the last tagged release is v0.9.0 (2019). |

### 2.8 Compatible extension points (for isolation controls)

**Compose layer**
- `x-*` keys are allowed wherever user-defined keys are not expected [CS 11-extension.md:L5-9]. compose-go keeps them in `Extensions` tagged `json:"-"` [CG types/types.go:L145,L752]. Stock compose therefore accepts them, **never sends them to the engine and never hashes them**, so changing an `x-shards-*` value does not recreate anything.
- So `x-shards-*` can only be an authoring surface for **shards' own compose**. Our compose must translate those keys into carriers the engine can see, so that recreation follows the hash.
- Any non-`x` unknown key is a validation error in stock compose [CG schema/compose-spec.json:L94-95; loader.go:L525-529]. Never invent keys.
- Carriers that stock compose forwards, all of which are hashed [CO create.go:L257-446,L732-844,L1430-1480]:
  - `labels` → `Config.Labels`;
  - `annotations` → `HostConfig.Annotations`;
  - `runtime`, `security_opt`, `sysctls`;
  - `gpus`, CDI `devices`, and reservation `devices` → `DeviceRequests{Driver,Capabilities,Options}`;
  - network `driver`, `driver_opts`, `internal`;
  - volume `driver`, `driver_opts`.

**Engine API layer**
- Unknown body fields are silently ignored [SW:L65-70; M httputils.go:L62-87; runconfig/config.go:L87-97]. Stock clients cannot send them anyway, and a real Docker engine would **drop them without error**. That is dangerous for security controls: a shards-aware client talking to real Docker would wrongly believe isolation was applied.
- Prefer existing namespaced maps:
  - `HostConfig.Annotations` (API 1.43+, CLI `--annotation` [CLI container/opts.go:L324-325]). moby passes annotations into the OCI spec [M daemon/oci_linux.go:L1041].
  - Labels.
  - `DeviceRequest{Driver,Options}` (CLI `--gpus driver=…,options=…` [CLI opts/gpus.go:L66-89]). Custom driver names are legitimate, and an unknown driver fails closed [M devices.go:L81-84].
- Fail-closed selectors:
  - `HostConfig.Runtime`: Docker rejects unknown runtimes with 400 [M daemon/runtime_unix.go:L219]. Advertise isolation classes in `Info.Runtimes`.
  - `SecurityOpt`: unknown keys are rejected [M daemon/daemon_unix.go:L234-257].
- Reserved namespaces to avoid:
  - `com.docker.compose.*` labels;
  - `org.opencontainers.*` annotations [RS config.md:L727-740];
  - `com.docker.*`, `io.docker.*`, `org.dockerproject.*` daemon labels [M daemon/pkg/opts/opts.go:L329-343].
- Responses may carry extra properties and headers (open schema). Use extra `/info` fields, `Info.Runtimes`, or `/version` `Components` entries to advertise capabilities.

**CLI layer**
- Plugins are executables named `docker-<name>`, where the name matches `^[a-z][a-z0-9]*$` and may not duplicate a builtin command or alias [CLI cli-plugins/manager/plugin.go:L64-133,L181-195].
- The **code** searches plugin dirs in this order: `cliPluginsExtraDirs`, then `$DOCKER_CONFIG/cli-plugins`, then `/usr/local/lib`, `/usr/local/libexec`, `/usr/lib` and `/usr/libexec` `…/docker/cli-plugins` [manager.go:L45-54; manager_unix.go:L15-19]. The code comment gives a different order.
- `docker-<name> docker-cli-plugin-metadata` must print JSON with `SchemaVersion` (`"0.1.0"`; majors below 2 are accepted since CLI 28.4.1) and a required `Vendor`, plus optional `Version`, `ShortDescription`, `URL` and `Hidden` [cli-plugins/metadata/metadata.go:L1-38; plugin.go:L64-127,L141-157].
- Plugins receive `DOCKER_CLI_PLUGIN_ORIGINAL_CLI_COMMAND` and `DOCKER_CLI_PLUGIN_SOCKET` [metadata.go:L17-21; socket/socket.go:L19].
- Hooks are configured under `plugins.<name>.hooks` and `error-hooks` and invoke `docker-cli-plugin-hooks`. They can only print hints [manager/hooks.go:L56-175].
- `aliases.builder` redirects `docker build` [CLI cmd/docker/builder.go:L67-80]. **Compose, however, hard-codes the plugin name `buildx`** [CO build_bake.go:L64].

**API version negotiation**
- Advertise only a real numeric `Api-Version`. Clients never go above their own max, so new features cannot be gated on a version bump.
- Advertising a version above a client's max is harmless, but then every version in [min, max] must be served with its version-gated behaviours.
- A non-numeric version breaks negotiation. The client silently stays at its max and retries on every request [M client/utils.go:L50-59; client.go:L303-325].

## 3. Implications for shards

### 3.1 Prioritized checklist

**P0: agents' typical compose stacks and docker CLI workflows work**

| # | Requirement | Contract refs | Proven by |
|---|---|---|---|
| 1 | Negotiation: `/_ping` GET+HEAD with the 5 headers; `Server`/`Api-Version`/`Ostype` on every response; versioned and unversioned routes; accept 1.40–1.56 with version gates; JSON errors and exact status mapping | §2.1 | moby integration/system; docker-py |
| 2 | Hijack: 101 UPGRADED, both content types, CloseWrite half-close, stdcopy including systemerr, raw TTY, resize | §2.1 | moby integration/container (attach, exec); docker-py |
| 3 | Container lifecycle: create (full Config/HostConfig/NetworkingConfig, multi-endpoint), inspect (leading `/`, `State.Health`), list filters, start/stop/kill/restart/rm/**rename**/wait (early headers, 3 conditions)/logs/top/stats/pause/update/prune | §2.1, §2.2 | moby integration/container; compose e2e |
| 4 | **Attach before start** loses no output. Archive HEAD/GET/PUT works on created containers. | CO up.go, secrets.go | compose e2e (up, configs, secrets, cp) |
| 5 | Start-error strings produce exit codes 126/127 | CLI run.go:L322-356 | cli e2e container |
| 6 | Exec create/start/inspect (ExitCode) | §2.1 | moby integration/container; cli e2e |
| 7 | Images: pull with NDJSON `\r\n` progress and in-band errors; token auth; Hub normalisation; OCI and schema2 manifests/indexes; skip `unknown/unknown`; list/inspect/tag/rm | §2.1, §2.6 | moby integration/image; cli e2e image |
| 8 | Networks: user bridges; `internal`; labels; IPAM; connect/disconnect with aliases; **embedded DNS for container name, service name and aliases**; 409 on duplicates | CO create.go | compose e2e networks |
| 9 | Volumes: local driver, labels, anonymous volumes and image `VOLUME` semantics, binds, tmpfs | SW, IS conversion | moby integration/volume |
| 10 | Events: create/start/die(exitCode)/destroy/health_status; Attributes include labels; label/type filters; NDJSON | §2.1 | compose e2e (up, logs -f, run hooks) |
| 11 | `/info`: `Swarm.LocalNodeState=inactive`, OSType, Runtimes. `/version`. | CO compose.go:L486-499 | moby integration/system |
| 12 | Engine-run health checks with exact defaults and health_status events | M health.go | compose e2e healthcheck, wait |
| 13 | GPU: DeviceRequests with nvidia/amd/cdi matching; compose `gpus` and reservations | §2.1, CS deploy.md | own GPU tests (no upstream suite runs without hardware) |
| 14 | Compose model: strict schema with `x-*`, interpolation incl. `:+`/`+`, multi-file merge, profiles, env_file, `depends_on` conditions, project-name rules | §2.5 | `compose config` differential; compose e2e |
| 15 | Compose runtime: identical labels and names; recreate triggers; configs/secrets; `run --rm`; `exec`; `cp`; `logs -f`; `up -d/--wait/--build`, `down -v`, `ps`, `ls` | §2.2 | compose e2e (`COMPOSE_E2E_BIN_PATH` = ours) |
| 16 | CLI: run/create/start/stop/rm/ps/logs/exec/cp/inspect/images/pull/push/build/tag/rmi/network/volume/system/events/wait/kill/restart/stats/top/port/login/context; `--format` via Go text/template; env vars; config.json incl. credential helpers | §2.3 | cli e2e (our binary in `./build`) |
| 17 | Build path for compose and `docker build`: a `buildx`-named plugin implementing `build` and `bake --file - --progress rawjson --metadata-file`, **or** BuildKit Control gRPC on `/grpc` + `/session` | §2.1, §2.2 | compose e2e build; BK tests via the shim |
| 18 | Dockerfile core: all instructions, parser directives, exec vs shell form, heredocs, lexer semantics, ARG scoping and platform ARGs, multi-stage with `--target`, COPY/ADD from context, RUN `--mount=cache,secret,bind`, `.dockerignore`, content-hash cache | §2.4 | BK golden tests, then BK dockerfile tests |
| 19 | `# syntax=docker/dockerfile:1[.x]` accepted without running a frontend image (built-in, as with `BUILDKIT_SYNTAX=dockerfile.v0`) | BK reference.md | BK tests with syntax headers |
| 20 | `use_api_socket`: engine socket at `/var/run/docker.sock` inside the VM, owned by the unprivileged user | CO apiSocket.go | compose e2e |

**P1**
- Every version-gated behaviour from 1.40 to 1.56.
- `attach/ws`, `export`, `changes`, `commit`, save/load (`/images/get`, `/images/load`), `system/df`, `build/prune`, `/auth`, `/distribution/{name}/json`.
- BuildKit parity: ssh/tmpfs mounts, `--network`/`--security` with entitlements, `RUN --device`, ADD git/URL/`--checksum`/`--unpack`, COPY `--link`/`--parents`/`--exclude`, named contexts, multi-platform, cache import/export, `# check`.
- Compose: `extends`, `include`, `watch` (tar sync, and exec `rm -rf` in the container), hooks, `scale`, `--exit-code-from`.
- Byte-identical `ServiceHash`, so stock compose and our compose do not recreate each other's containers.
- Push with chunked upload, cross-mount and referrers fallback.
- Exact image→runtime conversion, including the unknown-user error.
- Events as `json-seq`.

**P2**
- Swarm/stack endpoints and managed/authz plugins; checkpoints.
- The experimental JSON logs format (≥ 1.54); `/images/{name}/attestations`; image identity fields.
- `docker manifest` and trust; `ssh://` contexts (needs `system dial-stdio`).
- Windows.

### 3.2 Design constraints implied

1. **One socket, two protocols.** Stock buildx needs the HTTP/1.1 hijack (`Upgrade: tcp`, 101) and h2c/HTTP-2 gRPC on the same socket. Otherwise shards must ship its own `docker-buildx` plugin, because compose looks it up by that name.
2. **Flush discipline is semantic.** Wait and logs send their headers early. Progress and log errors are in-band after the first flush.
3. **Error text is API.** Exit codes 126/127 come from substrings, and compose treats 409/404 specially. The in-VM runtime must emit runc-compatible error phrases.
4. **Readiness latency floor.** Stock compose polls dependencies every 500 ms. Health defaults are interval 30s, start_interval 5s. The "≤5 ms ready" target cannot include `service_healthy` gating without changing user config. Our compose may be event-driven, but its observable semantics must stay identical.
5. **Attach-before-start** means output capture must begin before the process execs.
6. **Rootless in the VM, Docker paths outside.** `use_api_socket` hard-codes `/var/run/docker.sock`, which is also the client default. The in-VM init should create that path for the unprivileged engine.
7. **Go-isms leak into the contract.** `--format` is Go `text/template` plus its function map, and table output is tabwriter(10,1,3). `ServiceHash` is Go `encoding/json` of a compose-go struct. Any non-Go implementation needs faithful ports and golden tests.
8. **Extensions** are carried in namespaced labels and annotations (hashed, so they drive recreation), `Runtime` (fails closed), and `DeviceRequest{Driver,Options}`. `x-shards-*` is authoring-only. No new body fields without a capability check.
9. **Image store semantics.** Docker 29 defaults to the containerd store, which changes image IDs and multi-platform listings. Pick one model and test it against CLI output.

## 4. Open questions needing our own verification

1. Implement BuildKit Control gRPC plus `/session` for stock buildx, or ship our own `docker-buildx` (bake JSON schema and `rawjson` progress format)? The bake JSON and rawjson formats are **UNVERIFIED**; they live in docker/buildx.
2. Can we reproduce `ServiceHash` byte-for-byte (Go JSON field order, omitempty, HTML escaping)? If not, document the cross-tool recreate behaviour.
3. Under the containerd image store, which digest does `docker images`/`inspect` show as the image ID? **UNVERIFIED** against the code path.
4. Minimum API version: 1.40 (the moby default) or 1.44? docker-py's default version is **UNVERIFIED**.
5. The docs version matrix (29.8 → 1.55) vs source (1.56): re-check at implementation time.
6. The compose e2e `copyLocalConfig` inverted condition (framework.go:L125-135): confirm the runtime behaviour.
7. Triage the 132 remote-daemon skips and 1354 integration-cli tests into black-box and internal buckets.
8. runtime-tools needs root. Is running it inside a disposable test VM acceptable under the "no root" rule?
9. How do host `docker` clients reach the in-VM socket (vsock bridge vs `ssh://` + `dial-stdio`), and what `Server`/`Version` identity does shards report?
10. Should the `# syntax=` policy for non-`docker/dockerfile` frontends be an error or a warning?

## 5. References

- moby/moby `docker-v29.8.1` (464cd50c3d9e92877d56940ea160de6fca7bea23): https://github.com/moby/moby/tree/docker-v29.8.1. Files cited: api/swagger.yaml, api/docs/CHANGELOG.md, api/pkg/stdcopy, api/types/jsonstream, client/{client,ping,hijack,utils,envvars,client_unix}.go, daemon/server/**, daemon/{devices,devices_*,cdi,health,events,monitor,oci_linux,runtime_unix,daemon_unix,image_store_choice,errors}.go, daemon/config/config.go, daemon/pkg/registry/config.go, daemon/pkg/opts/opts.go, daemon/command/httphandler.go, internal/testutil/**, TESTING.md, hack/make/{test-docker-py,.integration-test-helpers}.
- moby client `client/v0.5.1`: https://github.com/moby/moby/blob/client/v0.5.1/client/client.go
- Engine API docs: https://docs.docker.com/reference/api/engine/ (fetched 2026-09-28)
- docker/cli `v29.8.1` (4a63305d74332de5ceba7fcbccbc3cbb7412f5ba): https://github.com/docker/cli/tree/v29.8.1. Files cited: cli/command/container/{run,create,utils,opts}.go, cli/command/cli.go, cli/command/formatter/**, templates/templates.go, cli/config/configfile/file.go, cli-plugins/**, cmd/docker/builder.go, cli/connhelper/connhelper.go, opts/gpus.go, docs/reference/{run.md,commandline/docker.md,commandline/login.md}, e2e/compose-env.yaml, scripts/test/e2e/run, internal/test/environment/testenv.go, TESTING.md.
- CLI reference: https://docs.docker.com/reference/cli/docker/ ; https://docs.docker.com/reference/cli/docker/container/run/ ; https://docs.docker.com/engine/containers/run/ (fetched 2026-09-28)
- docker/compose `v5.5.1` (5f94fb0aa42a2cd1248c6e6c7fafb87546b9c8de): https://github.com/docker/compose/tree/v5.5.1. Files cited: pkg/api/{labels,api}.go, pkg/compose/{hash,reconcile,create,service_containers,monitor,filters,up,attach,logs,build,build_bake,build_classic,run,exec,secrets,apiSocket,watch,compose,loader,api_versions}.go, internal/sync/tar.go, pkg/e2e/framework.go, Makefile, docs/reference/compose.md, go.mod.
- compose-spec/compose-go `v2.15.0` (4ddbf11f8bd3da1e9276f9185bad14596d388ece): https://github.com/compose-spec/compose-go/tree/v2.15.0. Files cited: schema/compose-spec.json, schema/schema.go, loader/{loader,normalize}.go, types/types.go, template/template.go.
- compose-spec/compose-spec `914ec15d1fa498969c0df5c1d672306db3256089`: https://github.com/compose-spec/compose-spec/tree/914ec15d1fa498969c0df5c1d672306db3256089. Files cited: 02–15 *.md, build.md, deploy.md, develop.md. Also https://docs.docker.com/reference/compose-file/extension/ (fetched).
- moby/buildkit `v0.33.0` (dddd5621af04ea57823085c93a063383f71d3173): https://github.com/moby/buildkit/tree/v0.33.0. Files cited: frontend/dockerfile/docs/reference.md, frontend/dockerfile/shell/lex.go, parser/testfiles, cache/contenthash/tarsum.go, docs/dev/solver.md, docs/attestations/attestation-storage.md, api/services/control/control.proto, util/testutil/{workers/dockerd.go,dockerd/daemon.go}. Also https://docs.docker.com/reference/dockerfile/ (fetched).
- docker/docs `f22c0e6595ca1996d2a6559cadcf2596499e6c11`: https://github.com/docker/docs/blob/f22c0e6595ca1996d2a6559cadcf2596499e6c11/content/manuals/build/concepts/context.md
- distribution/distribution `v3.1.2` (3220848f15d9279c66a41aa0a257469e6aede1e9): https://github.com/distribution/distribution/tree/v3.1.2/docs/content/spec (auth/token.md, auth/oauth.md, manifest-v2-2.md)
- opencontainers/runtime-spec `v1.3.0` (92249139eea7161e13745abd4cb6d0ea02a3227a): https://github.com/opencontainers/runtime-spec/tree/v1.3.0 (config.md, config-vm.md)
- opencontainers/image-spec `v1.1.1` (147f9c13cedb47a0c4d9a11a222961073d585877): https://github.com/opencontainers/image-spec/tree/v1.1.1 (conversion.md, image-index.md, media-types.md)
- opencontainers/distribution-spec `v1.1.1` (a139cc423184af6078077b9b7ee336eddbd03f8f): https://github.com/opencontainers/distribution-spec/tree/v1.1.1 (spec.md, conformance/README.md)
- opencontainers/runtime-tools `8a4db579f5c88af5a0d036fad34bddc9c1f703f3`: https://github.com/opencontainers/runtime-tools/tree/8a4db579f5c88af5a0d036fad34bddc9c1f703f3 (README.md)
- docker/buildx `v0.37.1` (0b265a9f62db554fa9aba6dd19e1bd5704bc7d8a): https://github.com/docker/buildx/blob/v0.37.1/driver/docker/driver.go (official Docker source outside the enumerated list; one fact)
- docker/docker-py `f387c3f92513189bbfbf7a77148138056d552e91`: pinned by moby's hack/make/test-docker-py. **Not retrieved.**
