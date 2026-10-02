# Agentfile: OCI-compliant microVM builds, and directives for agents

**Status:** requirements, as stated on 2026-09-29 (`EXPOSE` revised the same day). They are to be examined for gaps and
designed once the fixes from the 2026-09-29 audit are done. Nothing here is built yet.
Open questions noticed while recording them are listed at the end, for that review. The
design decisions that follow will go in `docs/design/architecture.md`, each with its
evidence.

shards microVMs build on Firecracker's model, and they must be OCI objects like any other.
A spec-compliant Dockerfile builds one. A small set of added directives declares the agents
a microVM runs, their skills and MCP servers, the harnesses that drive them, their volumes, and
the networks between them.
Everything is denied by default.

## 1. OCI compliance

- A shards microVM is fully OCI compliant. It is stored, recalled and managed as any other
  OCI container object: pushed to and pulled from registries, tagged, inspected.
- shards microVMs work in any OCI-compliant environment: Docker, Docker Compose, Kubernetes
  and the like.
- They work with OCI-compliant facilities such as OCI volumes.

## 2. Builds: `shards build` and the full Dockerfile vocabulary

- A user writes a fully spec-compliant Dockerfile, and shards ingests it and creates the
  VM from it. Of extreme importance.
- `shards build` builds microVMs as Docker builds images:
  - layered;
  - cacheable, both by layer and as a whole image;
  - over the full Dockerfile vocabulary.
- The directives below extend that vocabulary.

## 3. Default deny

A microVM without `EXPOSE` directives is intentionally airgapped and isolated. By default it
exposes no ports: a user exposes one with the CLI or with `EXPOSE`.

Today's microVMs already fit this: they have no network device at all. The host reaches the
guest over vsock alone (architecture.md D12).

Default deny covers files and processes as well as ports. Nothing a build step adds, and
nothing an agent or harness writes or runs, may reach past its own directory and grants,
directly or through another component (§9).

## 4. Directives

### 4.1 `EXPOSE`, with `AS` and `FOR`

```
EXPOSE <port> [AS <egress|ingress>] FOR [<network_name_a> <network_name_b> ...]
```

`[]` marks what is optional.

- **`AS`** limits the port to one direction:

  | Directive | Opens port 3000 for |
  |---|---|
  | `EXPOSE 3000` | ingress and egress |
  | `EXPOSE 3000 AS ingress` | ingress only |
  | `EXPOSE 3000 AS egress` | egress only |

- **`FOR <network_name_a> <network_name_b> ...`** lets the named networks (§4.6)
  communicate beyond the microVM on that port, the microVM's port, as its `AS` allows.

This extension is intentional, and part of the default deny.

### 4.2 `AGENT`

```
AGENT <name> FROM <source> [TO <path>]
```

- There is no `AS` (§12.4): the name comes first. `<source>` is read as §12.2 reads it.
- `<name>` is required. It names the agent, as a build stage's alias names an image.
  `AGENT main FROM some.registry.com/claude-opus-5-5` declares an agent called `main`.
- `<registry>` must name, in a valid OCI-compliant registry, a tarball that contains the
  agent. How that tarball is laid out is still to be specified.
- When a build meets an `AGENT` directive, it downloads the tarball and decompresses it to:
  - `TO <path>`, if given;
  - otherwise `/agents/<name>` (§12.1).
- Declaring an agent also means the build prepares the agent's workload/workspace in the
  VM: the containerd-like environment shards runs its agents in (see §6).
- A workspace is entirely isolated by default. This restrictive set is intentional:
  - no network egress or ingress;
  - no access to any volume, or any path, outside its own directory and that
    directory's subdirectories;
  - read-only permissions.

### 4.3 `SKILL`

```
SKILL [OPTIONS...] <path_or_link> [<dest_path>] [FOR <agent_name>]
```

- `SKILL` acts much like `ADD` by default. It finds and copies only valid skill markdown
  files, writing them to the destination path.
- Its options are every option `ADD` supports. For example, `--from` copies a skill added in
  an earlier stage or layer.
- It detects the kind of source as `ADD` does, and supports:
  - git repositories;
  - OCI tarballs, decompressed to the output path;
  - http(s) endpoints;
  - local paths.
- By default a skill is available to every workspace and every agent, in each agent's
  `/agent/<name>[_<tag>]/skills/`.
- `FOR <agent_name>` makes the skill available to that agent alone. For example, with
  `AGENT main FROM some.registry.com/claude-opus-5-5`, the directive
  `SKILL ./my_skill.md FOR main` gives the skill to `main` and no other agent.

### 4.4 `MCP`

```
MCP <name>[:<tag>] FROM <path | uri | url | git | oci_artifact>[:<version>] [FOR <agent_name>]
```

`MCP` declares an MCP server, local or remote, that the agents connect to.

**A URL or URI** is a remote server:
- Without a port, the port is the URL scheme's own: 443 for `https`, 80 for `http`
  (decided 2026-10-02). HTTPS is respected.
- The agents' workloads/workspaces are opened to receive traffic on that port, and so is
  the microVM: exposed for ingress and egress.

**A path, git URL or OCI artifact** is a server spoken to over stdio. Its `[:<version>]`
names the version fetched: a tag (or digest) for an OCI artifact, a commit SHA for a git
URL. Agents are opened to
send and receive its messages over stdio. The server is fetched as its source says:
- a git URL is cloned;
- an OCI artifact is downloaded and decompressed;
- a path is copied from the build host's files, as `COPY` copies.

Where each goes, and who may use it:

| | Without `FOR` | With `FOR <agent_name>` |
|---|---|---|
| Path, git or OCI server | `/mcp/<name>[:<tag>]`; every agent, workload and workspace | `/agent/<agent_name>[_<tag>]/mcp/<mcp_name>[_<tag>]`; stdio for that agent alone |
| Remote server | every agent | network access for that agent's workspace (and, necessarily, the microVM) alone |

With `FOR` on a remote server, an agent not named gets no network access to that server, its
ports or its network locations. Networking stays granular and tightly scoped.

### 4.5 `VOLUME`, with options and `FOR`

```
VOLUME [OPTIONS...] <path> [<dest>] [FOR <agent_name>]
```

- Without `FOR`, the volume, mount point or path is available to every workspace and agent.
- With `FOR <agent_name>`, it is available to that agent's workload/workspace alone.
- Options:
  - `--chown=<agent_name>/<user_id>/...`;
  - `--chmod=<permissions>`;
  - `--target-kind=<agent|harness>` (added 2026-10-01): whether the names after `FOR`
    are agents or harnesses (§4.10). A `VOLUME` without `FOR` is Docker's own.

### 4.6 `NETWORK`

```
NETWORK [OPTIONS...] <name> [FOR <agent_name_a> <agent_name_b> ...]
```

- It creates a network with the given configuration and name, as Docker Compose creates
  its networks.
- Its options are every option a Docker Compose network supports: driver, IPv4, IPv6,
  subnet and so on. Each option's default matches Compose's, functionally and in effect.
- Three more options open ports, on the microVM and on the network:

  | Option | Opens the port for |
  |---|---|
  | `--expose <port>` | ingress and egress |
  | `--ingress <port>` | ingress only |
  | `--egress <port>` | egress only |

- Without `FOR`, every agent may attach to the network. Agents attached may communicate
  with one another, and reach whatever ports the network's ingress and egress allow.
- With `FOR`, only the agents named may attach.
- **Declaring a network attaches no agent**, named or not. It creates the network, exposes
  the ports, and sets which agents may attach.

### 4.7 `CONNECT`

```
CONNECT [OPTIONS...] <agent> [<agent> ...]
    (WITH <agent> [<agent> ...] | TO <agent> [<agent> ...])
    ON <network> [<network> ...]
```

- Exactly one of `WITH` and `TO` is required.
- `--target-kind=<agent|harness>` (added 2026-10-01) says whether the names are agents
  or harnesses (§4.10).
- Several agents may follow `CONNECT`, `WITH` and `TO`, and several networks may follow
  `ON`.
- `WITH` connects them both ways, for every agent attached to the networks: each may send
  requests to the others and answer theirs.
- `TO` still attaches every agent named to the networks, but only one way:
  - agents after `CONNECT` may send requests to agents after `TO`, and receive their
    responses;
  - agents after `TO` may not send requests to agents after `CONNECT`; they may only
    respond.

### 4.8 `HARNESS`

Added 2026-10-01, and revised the same day: a harness has no agents until `ATTACH` grants
them.

```
HARNESS <name> FROM <source>[:<tag>] [TO <dest_path>]
```

- `<name>` is required, and names the harness.
- `FROM <source>[:<tag>]` names where the harness comes from, with an optional tag, as
  `AGENT` does. The build detects the kind of source and fetches the harness that way:

  | Source | The build |
  |---|---|
  | git URL | clones it |
  | http(s) URL | downloads it, respecting HTTPS |
  | path on the build host | copies it, as `COPY` copies |
  | OCI reference | pulls it from its registry |

- A harness is a file artifact. The build unpacks it to `/harness/<name>`, or to
  `TO <dest_path>` if given.
- **It has no agents** by default. `ATTACH` (§4.9) grants it agents.
- **`VOLUME` and `CONNECT`** name harnesses as they name agents, told apart by
  `--target-kind`
  (§4.5, §4.7). That is how a harness is granted volumes and paths and attached to
  networks.
- **Deny by default.** A harness is held to the same deny-by-default rules and file
  permissions as an agent (§4.2, §8): no network, nothing outside its own directory, and
  read-only except where a directive grants more.

### 4.9 `ATTACH`

Added 2026-10-01. `ATTACH` is the working name; the user proposed `ENABLE`, then
`ATTACH`.

```
ATTACH <agent_a> [<agent_b> ...] FOR <harness_a> [<harness_b> ...]
```

- It is a permission and nothing more: every harness named may access every agent named.
  Installing and using the agent is the harness's own work, for example with short
  scripts.
- Without it, a harness can access no agent. Grants are explicit, in lines of their own,
  apart from where agents and harnesses are fetched, and they do not depend on the order
  of the file.
- It names only agents after `ATTACH` and only harnesses after `FOR`, so its names are
  never ambiguous.

### 4.10 Names of agents and harnesses

Agents and harnesses may share a name: users do unexpected things, and the build must
never resolve a name to the wrong kind. Where a directive may name either (`VOLUME`,
`CONNECT`), `--target-kind` is optional. It is not `--type`, which reads as a mount's
type (`--mount type=bind`, Compose's `type:`), nor `--scope`, which Docker's volume
drivers use for `local` and `global`:
- without it, the build determines each name's kind: a name that belongs to one kind
  alone resolves to that kind;
- a name that belongs to both kinds (a `HARNESS main` and an `AGENT main`) is a build
  error unless `--target-kind` says which is meant. It never defaults to one kind.
- That error helps the user fix it. It names the directive and its line, the ambiguous
  name, and where each of the agent and the harness of that name is declared, and it shows
  the directive rewritten both ways, for example:

  ```
  Agentfile:12: VOLUME ./data /data FOR main: "main" names both an agent and a harness
    AGENT main is declared at line 3
    HARNESS main is declared at line 7
  Say which with --target-kind:
    VOLUME --target-kind=agent ./data /data FOR main
    VOLUME --target-kind=harness ./data /data FOR main
  ```

## 5. Communication between agents in a microVM

Networks between agents need more than the directives:

- The microVM must let workspaces communicate readily, and make its agents aware of how.
- The design must be deny-by-default and encrypted.
- Ideally each microVM runs a small server for this, and its agents are made aware of the
  server and of how to use it.
- A code-mode MCP server may be ideal for that: every agent declared with `AGENT` discovers
  it by default.

## 6. Relation to what exists

- **Images.** `shards pull` and `shards run IMAGE` take OCI images from registries
  (`crates/registry`) and build a root filesystem from their layers (`crates/image`).
  `shards build` builds Dockerfiles as BuildKit does (architecture.md D33); the
  directives here are not built yet.
- **Networking.** No network device exists. A microVM is reached only over vsock, so it is
  airgapped today, the default this spec keeps. `EXPOSE`, `NETWORK`, `CONNECT` and remote
  `MCP` servers will reach the network through a network process of the microVM's own, never
  through the VM process, which stays confined without TCP; that process holds the policy
  they declare (architecture.md D31).
- **Agents' workspaces.** The containerd-like runtime inside each microVM runs many agents
  per VM, each isolated as a container would be, without being containers. It is where the
  workloads/workspaces of §4.2 live. Mounts go to one workload, some or all, attached once
  (architecture.md D17). That is how `VOLUME ... FOR`, `SKILL ... FOR` and `MCP ... FOR`
  would reach their agents, and `ATTACH` its harnesses.
- **Isolation per agent and per microVM**, of network, devices and permissions, is already
  a requirement of that runtime, and `EXPOSE`, `NETWORK` and `CONNECT` extend it.

## 7. Open questions, for the review

Recorded as found. The answers the review has given so far are in §8.

1. **The agent tarball.** Its layout was to be explained, and has not been yet: what it
   holds, how shards runs what is in it, and how it relates to an OCI image or artifact.
2. **`AS` in `AGENT`.** Where does the optional `AS` go: `AGENT [AS] <name> FROM ...`, or
   `AGENT FROM <registry> AS <name>`, as `FROM ... AS` names a stage?
3. **`/agents` or `/agent`.** An agent unpacks to `/agents/<name>[_<tag>]`, but its skills
   and MCP servers go under `/agent/<name>[_<tag>]/`.
4. **`:` or `_` in MCP paths.** Unscoped servers go to `/mcp/<name>[:<tag>]`, a colon in a
   path, and scoped ones to `.../mcp/<mcp_name>[_<tag>]`.
5. **"A local remote MCP server that all agents must connect to."** Must every agent in
   scope connect to it, or may they? And are local and remote servers both meant?
6. **MCP ports.** Is port 8000 the default for `https://` URLs too, or does HTTPS keep 443?
   What does `[:<port>]` mean after a path, git URL or OCI artifact?
7. **`EXPOSE`.**
   - Is `FOR` required, as the syntax as given reads, or optional? What may use a port
     exposed with no network named: nothing yet, the microVM alone, every agent?
   - Is an egress port the destination port of outgoing connections, to any
     destination?
   - How do `AS` and Docker's protocol suffix (`EXPOSE 3000/udp`) combine? And how does
     `docker run -p` map onto ingress and egress?
   - How do `EXPOSE ... FOR <network>` and a network's own `--expose`, `--ingress` and
     `--egress` (§4.6) relate: two ways to open the same port, or different things?
8. **`NETWORK`'s port options.** The text named `--egress` for "both egress and ingress"
   and again for "egress only". §4.6 reads the first as `--expose`.
9. **A workspace's "read-only permissions".** Read-only everywhere, its own directory
   included? Where may an agent write, and how is that granted?
10. **`VOLUME <path> [<dest>]`.** A Dockerfile's `VOLUME` names a mount point in the image;
    its source comes at run time (`-v`, Compose `volumes:`). What is `<path>` beside
    `<dest>`: a build-time source, a named volume, a host path? How does it map to OCI
    volumes, and to Kubernetes volumes?
11. **`--chown=<agent_name>/<user_id>/...`.** What is its grammar, beside Docker's
    `--chown=<user>:<group>`?
12. **"Valid skill markdown files."** What makes a skill valid: a `SKILL.md` with its
    frontmatter, as in Anthropic's Agent Skills format, or another rule? Are a skill's
    other files copied with it?
13. **`CONNECT`'s options.** Which are they?
14. **`CONNECT ... TO` and a network's `FOR`.** If agents after `TO` are attached, must the
    network's `FOR` list allow them? What happens when it does not?
15. **Docker, Compose and Kubernetes.** Running "in any OCI-compliant environment" could
    mean images that other engines run as ordinary containers, a shards OCI runtime that
    they run as microVMs (a Kubernetes RuntimeClass, as Kata Containers provides), or
    both. And how do the added directives' results travel in an OCI image, as config,
    annotations or artifacts, so that other tools keep them?
16. **Building with Docker.** A file using the added directives is no longer a Dockerfile
    `docker build` accepts. Is it called an Agentfile? Should `docker build` build it too,
    through a BuildKit frontend named by a `# syntax=` line?
17. **The in-VM server.** What identities and keys encrypt and authorize its traffic?
    How is its reach tied to `NETWORK`, `CONNECT`, `MCP ... FOR` and `EXPOSE`? What does
    "code-mode" MCP mean exactly, and from which source?
18. **`HARNESS` and `ATTACH`.**
    - Does a harness run as a workload/workspace of its own in the in-VM runtime, as an
      agent does, with its own private scratch directory?
    - What does `ATTACH` let a harness do with an agent: send it requests and drive its
      runs (through the in-VM server of §5), read the agent's directory, or both? Reading
      it is an exception to the agents' total isolation (§8).
    - Is it an error for `ATTACH` to name an agent or a harness that was never declared?
    - The keyword: `ATTACH` already describes agents joining networks (§4.6, §4.7), and
      `docker attach` means joining a container's streams. Is a different word wanted,
      or does the networks' wording change?
    - Its artifact: is it its own OSI type (`application/vnd.osi.harness.v1`, beside
      `vnd.osi.agent.v1`)? And is "unzipped" a zip archive, or the tar+zstd layers agents
      use?
    - What does `[:<tag>]` mean for a git URL (a ref?), an http URL or a path? How does
      the build tell a tag from a colon in the source: a URL's port, `C:\` on Windows, a
      path with a colon in it?
    - One `--target-kind` covers every name in a `CONNECT`. How does a `CONNECT` join an
      agent with a harness when one of their names is ambiguous: two kinds of the flag,
      a qualifier per name, or not at all?
    - Do skills and MCP servers reach harnesses too, and do `SKILL`, `MCP` and `NETWORK`
      take `--target-kind`?
    - Does unpacking to `/harness/<name>` omit the tag, where agents go to
      `/agents/<name>[_<tag>]`?
19. **`COPY`, stages, agents and harnesses** (raised 2026-10-01). Each has a recommended
    answer, for the review to confirm:
    1. **`COPY --from=<agent or harness>`.** Recommended: allowed. Its root is the
       artifact's own content, as `--from=<image>`'s root is that image's root filesystem.
    2. **One namespace.** Recommended: stage aliases, agents and harnesses share one; a
       name used by two of them is a build error, with the help §4.10 gives, since
       `--from=main` would otherwise mean either.
    3. **Writing into an agent's or harness's directory.** Recommended: a build error for
       any `COPY`, `ADD` or `RUN` that is not that agent's or harness's own directive.
       Agents are signed artifacts their runtime keeps read-only, and a step that changes
       their files would change a signed artifact silently. `VOLUME ... FOR` and the
       scratch directory are how it gets more.
    4. **The layers of `AGENT` and `HARNESS`.** Recommended: each adds one layer as
       `COPY --link` does, independent of those below it, so it is cached alone and a new
       base image does not rebuild it.
    5. **Directives in stages.** Recommended: `AGENT`, `HARNESS`, `SKILL`, `ATTACH` and
       every grant belong to the stage they are written in, and the microVM has those of
       its target stage's lineage only. `COPY --from=<stage>` copies files, never a stage's
       agents or grants: a builder stage must not widen what the final microVM allows.
    6. **`SKILL --from=<agent>`.** An agent's config lists the skills it brings. May another
       agent take one (`SKILL --from=main <skill> FOR other`)? That gives one agent what
       another shipped, which the rule that agents cannot read one another (§8) has to
       allow explicitly. Open.

20. **Relays, declassifiers and MCP instances** (raised 2026-10-01).
    - The syntax that names an agent a relay for another (§9.5), and a declassifier: a
      `CONNECT` option, a directive of their own, or a grant on `NETWORK`?
    - How long a shared MCP server's instance for one caller lives (§9.6): one run of the
      agent, one session, or the microVM's life, and whether it keeps state between calls.

21. **Spawning** (raised 2026-10-01). How does an Agentfile declare that an agent or harness
    starts no process, or bound how many it starts (§9.9): an option of `AGENT` and
    `HARNESS`, or a grant of its own? And should `pids.max` have a default?

22. **Ingesting OCI images** (raised 2026-10-01, §10). Is the Agentfile made from an
    image one image's spec alone, or also a Compose file's services as agents of one
    microVM? Should an image's `RUN` history be offered as a rebuildable Agentfile, which
    needs its build context, or is `FROM` by digest the answer?

## 8. Answers from the review

Given 2026-10-01.

- **Q1, the agent.** An OCI artifact of its own type, not an OCI image: its
  `artifactType` and manifest are shards' to specify. Firecracker's existing integrations
  with OCI are the reference to study first.
  - **The shape** (docs/research/oci-artifacts.md §4): OCI 1.1's own-config-and-layers
    case, as Helm and Wasm are, under a new standard the user proposes, the **Open Sandbox
    Initiative (OSI)**: `artifactType` `application/vnd.osi.agent.v1`, config
    `application/vnd.osi.agent.config.v1+json` (name, version, how it runs relative to its
    directory, platform, the skills and MCP servers it brings, the capabilities it asks
    for, which an Agentfile grants or refuses), content layers
    `application/vnd.osi.agent.content.v1.tar+zstd` (or `+gzip`) rooted at the agent's
    directory, an index for several platforms, signatures and SBOMs as its referrers.
    Firecracker itself has no OCI support; firecracker-containerd runs container images in
    a VM and defines no artifact type. IANA's media-type registry (application, text and the other
    trees, fetched 2026-10-01) holds no `vnd.osi` type; "OSI" is otherwise the Open Source
    Initiative's acronym.
- **Q7, `EXPOSE` without `FOR`.** `FOR` is optional. Without it the port opens at the
  microVM's boundary only (reachable as `shards run -p` publishes it); no agent may use it
  until a `NETWORK` or `CONNECT` grants it.
- **Agents cannot read one another** (the user, 2026-10-01): total isolation is the
  default. No agent reads another agent's directory or anything another agent's workspace
  has written (its scratch directory included). Only a
  `VOLUME ... FOR`, `NETWORK` or `CONNECT` the Agentfile declares lets one agent reach what
  another holds, and the in-VM runtime enforces it, not the agents.
- **Q9, writes.** Everything an agent sees is read-only but a private scratch directory of
  its own, lost when its run ends; any other writable path is a `VOLUME ... FOR` grant.
- **Q12, skills.** The Agent Skills format: a directory whose `SKILL.md` has YAML
  frontmatter with `name` and `description`, copied whole with its other files; anything
  else is refused.
- **Q15, other engines.** Running shards-built microVMs from Docker, Compose or Kubernetes
  is wanted only if it keeps everything this spec defines working fully: the VM process's
  confinement (Landlock, App Sandbox), the networking of `EXPOSE`, `NETWORK` and `CONNECT`,
  and the other directives. If that cannot be done fully, other engines run them through
  shards' runtime alone. To decide, research how Docker and the others build and run
  microVMs today.
  - **Decided by that research:** shards' runtime alone (architecture.md D32,
    docs/research/oci-engines.md). The engines own the network namespace and port
    publishing outside any runtime's reach, App Sandbox cannot exist inside Docker
    Desktop's VM, and they run as root. Images and agents stay OCI objects in registries,
    and Agentfiles build with `docker buildx` through the frontend.
- **Q16, the file.** It is an `Agentfile` (a plain `Dockerfile` builds too). `shards
  build` reads both, and a BuildKit frontend named by a `# syntax=` line lets `docker
  buildx build` build an Agentfile as well.
- **Where the directives travel.** The normalized Agentfile, as a file in a layer of its
  own, is canonical: it survives every store and copy, and shards' runtime reads it. A
  config label carries its digest and a summary, manifest annotations make it findable in
  registries, and attestations serve provenance alone (docs/research/oci-artifacts.md §4).

## 9. Isolation, from build to run

Raised 2026-10-01. Confining an agent's writes to its own directory is not enough: a build
step can plant what reaches past a directory, and what an agent writes or runs can reach
past it through something with more access. Both are closed by the same rules, held at
build time and again at run time.

### 9.1 Domains

Every path in a microVM's filesystem belongs to one domain: the system, one agent (its
directory, `/agents/<name>[_<tag>]`, and what its directives add under
`/agent/<name>[_<tag>]/`), or one harness (`/harness/<name>`). Its scratch directory belongs
to it too. A domain holds only what its own directives put there.

### 9.2 Held at build time

The build knows where every file came from: each step runs on a snapshot of the tree
(`crates/build`). Before an image is exported, each domain is checked, and any of these
fails the build with the step's line, the path and why:

- a symlink whose target, resolved inside the image, leaves its domain (`-> /etc`,
  `-> ../other`), absolute or relative;
- a hard link whose names lie in two domains: path rules cannot see a hard link;
- a device node, FIFO or socket in an agent's or harness's domain, and a file there with
  set-user-ID, set-group-ID or file capabilities: `COPY --from` carries all of them over
  from a stage, and `--chmod` sets the bits;
- a path outside a domain owned by that domain's user (`--chown` to an agent's uid on
  `/etc/...`), and a path inside one owned by another domain's;
- a write into a domain by a step other than its own directive (§7 Q19.3).

### 9.3 Held at run time

What runs in a domain stays in it, whatever it starts: the confinement is the whole
process tree's, and nothing in it can drop it. Each agent and harness runs:

- in a mount namespace whose root holds its own domain and what it was granted, so a path
  outside resolves to nothing, whatever a symlink says;
- in a PID namespace of its own, so `/proc/<pid>/root` and ptrace reach no other domain's
  processes;
- in an IPC namespace of its own, so System V IPC and POSIX message queues reach no other
  domain's;
- under a user and group ID of its own, no other domain's, so the kernel's per-user
  objects (keyrings among them) are not shared;
- in a cgroup of its own, whose `pids.max` bounds its processes (§9.9);
- under Landlock rules, with `no_new_privs` set, so set-ID files gain it nothing, and a
  seccomp filter; Linux makes children inherit all three and none can be undone;
- with its scratch directory mounted `nosuid,nodev`.

A harness is a domain as an agent is, and runs under all of it. The guest kernel's
`kernel.io_uring_disabled` is set to 2 before any domain starts (Linux 6.6 and later): an
io_uring ring opens and connects sockets without the system calls a seccomp filter sees.
The pinned guest kernel has no Yama (`resources/kernel/`), so ptrace is closed by the PID
namespace and the separate IDs alone.

### 9.4 Crossing domains

What one domain produces reaches another (an `ATTACH`ed harness reading an agent's output,
an agent reading a `VOLUME` shared `FOR` both, the host's `shards cp`) only through what a
directive declares, and the reader never trusts its shape. It opens paths with
`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` or their equivalent, refuses FIFOs,
devices and sockets where it expects files, and reads with bounded sizes, so a symlink, a
FIFO or a giant file an agent planted cannot redirect, stall or exhaust a component with
more access.

### 9.5 What reaches past a domain through another

An agent attached to an internal-only network shared with an agent that may reach the
world can have that agent carry its data out, by asking it, or by injecting instructions
into what it reads. Each connection is allowed; the escape is in the second agent, whose
judgment is no control. Two rules close it, neither relying on an agent:

- **At build time, reach is transitive.** For every agent and harness, the build computes
  what it reaches through every edge: networks, `VOLUME ... FOR` shared, `ATTACH`, MCP
  servers. An internal-only agent with any path to one that may reach the world reaches
  the world, and that is a build error naming the path (`B -> network internal -> A ->
  network world`), unless the Agentfile names `A` a relay for `B` (§7 Q20). Authority
  flows along every edge, so the closure is checked, not each edge alone.
- **At run time, data carries labels** (information flow control: Flume, Krohn et al.,
  SOSP 2007; HiStar, Zeldovich et al., OSDI 2006). Every message through the in-VM server
  (§5) carries its sender's label. A process that receives internal-only data takes that
  label, and the egress gate refuses what carries it. An agent that serves others handles
  each request in a confined process of its own, labelled by that request alone, so serving
  `B` taints only the work done for `B`. Only a declassifier the Agentfile grants lets
  labelled data out.
- **Not covered:** covert timing channels (one agent modulating load or the timing of
  allowed requests). Labels at process granularity do not close them, and this spec does
  not claim to.

### 9.6 MCP tool calls

A tool runs with the grants of the agent or harness that called it, never more. An MCP
server is otherwise a deputy: an agent with no filesystem or network grants could reach
whatever the server reaches by asking it.

- **A local server declared `FOR` one agent** runs inside that agent's confinement, a child
  of its process tree, so its tools inherit everything of §9.3.
- **A server shared by several** never holds the union of their grants. Each caller gets an
  instance of its own, in its own confinement, made at its first call, so no call reaches
  another caller's grants, nor its data through the server's state (§7 Q20).
- **A remote server** is egress from its caller. Calling it takes the caller's own grant to
  reach it; under §9.5 an internal-only agent, or one holding internal-only data, calls none.
  Remote tools act outside the microVM: what shards holds is what the caller may send.
- **Results carry labels.** A tool's result carries its server's label joined with what it
  read for the caller, so what a tool fetched does not pass the rules of §9.5.

### 9.7 Network reach is the process tree's

Raised 2026-10-01: an agent without network access writes a script, and the script
connects to a port open on the microVM. A grant checked where the agent asks for something
misses whatever the agent runs; reach must be a property of the process tree, held by the
guest kernel, so that a script, a compiled binary or an interpreter one-liner has exactly
the agent's reach. Each agent and harness has:

- **A network namespace of its own.** One without network grants has only its own
  loopback: a port another domain listens on, or the network process (§6) serves, is in
  another namespace, and nothing routes to it. One with grants has one veth link to the
  network process, which applies its policy by the link a packet arrives on, never by its
  source address. The namespace covers every protocol, UDP and raw sockets among them, and
  abstract Unix sockets, which Linux keeps per network namespace (`net/unix/af_unix.c`,
  `unix_find_abstract(net, ...)`).
- **Landlock network rules**, for the TCP ports it may bind and connect to within its
  namespace (Linux 6.7), and scoping that refuses abstract Unix sockets and signals across
  domains (Linux 6.12). Landlock's UDP rights come only in Linux 7.2; the pinned guest
  kernel is 6.18.48, so the namespace alone holds UDP.
- **A seccomp filter on `socket()`** allowing only the address families its grants need.
  `AF_VSOCK`, `AF_PACKET` and `AF_NETLINK` are refused to every domain.
- **Its mount namespace** (§9.3), so a pathname Unix socket exists for it only when
  granted, as the in-VM server's of §5 is.

**vsock, today.** The guest reaches the host over vsock alone, and nothing yet keeps a
workload from opening `AF_VSOCK` itself: a connection to host CID 2 reaches the ports the
VM process serves, the run and signal ports (`crates/shards/src/vm_run.rs`), and with a
vsock path any port P reaches the host socket `<path>_P` (`crates/vmm`, the vsock muxer).
Only shards-init may open `AF_VSOCK`, and the host accepts each port once per run. Whether a
workload can use either port today is not yet tested; §9.10 holds the test.

### 9.8 One domain triggering another

Raised 2026-10-01: an agent without network access writes a script that triggers an agent,
or a harness, that has it. This is §9.5's deputy again, except that the trigger need not be
a request the deputy was declared to take: it can be any way one process affects another.
Two rules close it.

**Every channel that was not declared does not exist.** These are Linux's ways for one
process to affect another, and what closes each:

| Channel | Closed by |
|---|---|
| Signals, ptrace, `/proc/<pid>` | the PID namespace, the separate IDs, Landlock's signal scope (§9.3) |
| System V IPC, POSIX message queues | the IPC namespace |
| Shared memory (`/dev/shm`), FIFOs, files the other watches (inotify, fanotify) | the mount namespace |
| Unix sockets, pathname and abstract | the mount and network namespaces, Landlock's scope |
| TCP, UDP and raw sockets | the network namespace (§9.7) |
| Keyrings | the separate IDs, and seccomp refusing `keyctl`, `add_key` and `request_key` |
| vsock to the host, asking it to run something | only shards-init opens `AF_VSOCK` (§9.7) |
| cgroup files, perf events, `userfaultfd`, BPF | not mounted, or refused by seccomp |

**Every channel that was declared is an edge of §9.5.** What remains are channels the
Agentfile declared: a `VOLUME` shared `FOR` both, `CONNECT`, the in-VM server, `ATTACH`, a
shared MCP server. If one domain can trigger another, it reaches what the other reaches,
and the build's closure counts it. A harness is no exception: `ATTACH` is an edge both ways,
since the harness drives the agent and reads what it produces. So a harness attached to an
internal-only agent and to one that may reach the world joins the two, and the build fails
with that path, unless the Agentfile names the harness a relay (§7 Q20).

Run-time labels (§9.5) follow data only through what shards mediates, the in-VM server and
MCP results: Linux does not carry taint across a `read()` of a shared file. For files the
build's check is the guarantee, so a `VOLUME` shared by an internal-only domain and one that
may reach the world is a build error.

### 9.9 Knowing what a domain runs

Every process of an agent or harness is born in its PID namespace and its cgroup, inherits
both, and cannot leave them: moving a process between cgroups takes write access to the
cgroups' common ancestor (`kernel/cgroup/cgroup.c`, `cgroup_procs_write_permission`), and
no domain's mount namespace holds the cgroup filesystem. So the in-VM runtime, outside
every domain, knows each process a domain starts:

- **as it starts**, from the kernel's process events connector (`CONFIG_PROC_EVENTS`, on in
  the pinned guest kernel): fork, exec and exit, for a listener in the initial PID and user
  namespaces (`drivers/connector/cn_proc.c`), which the runtime is. It maps each to its
  domain by its cgroup. This is a record, for `shards top` and an audit trail; the
  confinement of §9.3 does not depend on it;
- **at any time**, from the domain's `cgroup.procs`, and whether anything of it still runs
  from `cgroup.events`;
- **at its end**: `cgroup.kill` ends every process of the domain at once, and ending the PID
  namespace's first process ends the rest, so a double fork leaves nothing behind.

A domain's spawning is bounded by `pids.max`. A domain declared to start no process at all
(syntax §7 Q21) runs with a seccomp filter that refuses `clone` without `CLONE_THREAD`,
`fork`, `vfork` and `execve`/`execveat`, and refuses `clone3` with `ENOSYS`, since its flags
lie in a structure a seccomp filter cannot read, so that libc falls back to `clone`.
Decisions are never made with seccomp's user notification: the kernel's documentation
warns that what a notification shows can change before the call runs (TOCTOU,
`Documentation/userspace-api/seccomp_filter.rst`).

Refusing `execve` does not stop a domain from running code: `bash script.sh` executes
nothing new, and an interpreter runs what it reads. So spawning is recorded and bounded, but
the boundary is the confinement every process of the domain is under, whatever it runs.

### 9.10 Tests

Each escape above has an end-to-end test in a real microVM that must fail closed: an agent
following a planted symlink, using a hard link, running a set-user-ID binary, reading a
sibling's `/proc` or tracing it; a harness handed a symlink, a FIFO and an oversized file;
an internal-only agent having a connected agent send its data out; an agent with no grants
asking a local MCP tool to read outside its directory, a shared server to read another
agent's files, and a remote server to send anything; an agent with no network grants whose
script tries TCP and UDP to every port on loopback and on the microVM's addresses, every
host vsock port, abstract and pathname Unix sockets, raw and packet sockets, and io_uring;
an agent and a harness that try each channel of §9.8's table on another; a domain that
double-forks and is still found and ended (§9.9), and one declared to start no process
that tries every way to start one; and Agentfiles that try each build-time escape of §9.2
and each transitive reach of §9.5 and §9.8, a harness attached to an internal-only agent
and a world-reaching one among them. A test also asks today's workloads to dial the host
over vsock (§9.7), before the rule that closes it exists. Each guard is mutation-checked:
the suite fails without it.

## 10. Any OCI image, as a microVM and as an Agentfile

Raised 2026-10-01. shards takes in any valid Linux OCI image and makes of it a microVM, an
Agentfile, or both. Linux only, as Firecracker runs Linux guests; other `os` values are
refused by name.

- **What exists.** `shards pull` and `shards run IMAGE` take an image from a registry:
  an index is resolved to this host's `linux/<arch>` manifest, its layers (gzip, zstd or
  uncompressed, with whiteouts) are applied, and the root filesystem is written as EROFS
  and booted, with the config's entrypoint, command, environment, user and working
  directory (crates/registry, crates/image, D25).
- **What it takes in.** Every way Docker and OCI tools hand an image over, not only a
  registry: an OCI image layout directory or its tar (`oci-layout`, `index.json`,
  `blobs/`, image-spec v1.1 image-layout.md), and the tar `docker save` writes
  (`manifest.json` and its layers), as `docker load` reads both. Each is verified by its
  digests before it is used, as a pull is.
- **What it makes.**
  - A microVM: the image's root filesystem, booted, its config applied as `docker run`
    applies it.
  - An Agentfile that builds the same image: `FROM` the image by digest, so that the
    build reproduces it exactly, with the config's settings written out as the
    instructions that set them (`ENV`, `USER`, `WORKDIR`, `ENTRYPOINT`, `CMD`, `EXPOSE`,
    `LABEL`, `STOPSIGNAL`, `HEALTHCHECK`, `VOLUME`, `SHELL`), and the image's history
    as comments where the image records it. Under default deny (§3), its `EXPOSE`d ports
    and `VOLUME`s become declarations the Agentfile shows for the user to grant, not
    grants.
- **Tests.** Each form taken in, from images Docker itself writes (`docker save`,
  `buildx --output type=oci`), boots and runs as `docker run` runs it; and the Agentfile
  made from each builds an image whose config is the original's.
- **Open (§7 Q22).** Whether "an Agentfile" means one image's spec alone, or also a
  Compose file's services as agents of one microVM; and whether an image's `RUN` history
  should be offered as a rebuildable Agentfile, which needs its build context, rather
  than `FROM` by digest.

## 12. Recommended answers to §7, for the review

Written 2026-10-02, as §11's last paragraph asks: each item is built on the answer here
until the review changes it. The first four fix the grammar the parser reads; the review decided them on 2026-10-02
(1, 2 and 4 as recommended, 3 as written there).

1. **Directories carry names, never tags (Q3, Q4, Q18).** An agent unpacks to
   `/agents/<name>`, a harness to `/harness/<name>`, an unscoped MCP server to
   `/mcp/<name>`, and what `SKILL … FOR` and `MCP … FOR` grant an agent under
   `/agents/<name>.d/{skills,mcp}/`. A tag may itself hold `_`, `.` and `-` (OCI
   distribution-spec's tag grammar `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`), so
   `<name>_<tag>` cannot be split back, and §4.10 already makes names unique. The
   version an agent came at lives in its config and in the normalized Agentfile. The
   grant directory stands beside the agent's own, not in it: the agent's directory is
   its signed artifact, read-only, holding only what the artifact holds (§9.1).
2. **A source's version is the source's own syntax (Q18).** An OCI reference keeps its
   `:tag` or `@digest`, as `FROM` reads one; a git URL names its ref and subdirectory as
   BuildKit's git contexts do (`url#ref:subdir`); an http(s) URL and a path take no
   version, and a `:<tag>` after them is an error. That ends the confusion of a tag
   with a URL's port, `C:\`, or a colon in a path.
3. **`MCP`'s sources (Q6; decided 2026-10-02).** A URL in `FROM` is always a remote
   server, and its port, when the URL names none, is its scheme's: 443 for `https`,
   80 for `http` (RFC 3986 §3.2.3, RFC 9110 §4.2). After a path, git URL or OCI
   artifact, `[:<version>]` names the version fetched: a tag (or digest) for OCI, a
   commit SHA for git (§4.4).
4. **`AGENT` has no `AS` (Q2).** `AGENT <name> FROM <source> [TO <path>]`: the name is
   first and required, as `ARG <name>` and `ENV <name>` name theirs. `AS` would give
   one directive two ways to name the same thing; `AGENT AS main FROM …` is refused,
   with the line rewritten.
5. **MCP servers are offered, not imposed (Q5).** Declaring one, local or remote, makes
   it reachable and discoverable through the in-VM server (§5) for the agents in scope;
   whether an agent connects is the agent's.
6. **`EXPOSE` (Q7).** An egress port is the destination port of outgoing connections,
   to any destination the networks allow. The protocol suffix stays on the port and
   `AS` follows it: `EXPOSE 3000/udp AS ingress`. `shards run -p` and `-P` publish
   ingress ports only; publishing a port declared `AS egress` is an error. `EXPOSE …
   FOR <network>` opens the port at the microVM's boundary for that network's members;
   `NETWORK --expose/--ingress/--egress` opens it on the network between its members.
   They are two boundaries, and a flow crossing both needs both.
7. **`NETWORK`'s port options (Q8)** are as §4.6 reads them: `--expose` both ways,
   `--ingress`, `--egress`.
8. **`VOLUME <source> <dest>` (Q10).** One argument is Docker's `VOLUME`, a mount point
   that gets an anonymous volume at run. Two name a volume by name, as Compose's
   `volumes:` does, never a host path: a build records no host path, and `shards run
   -v` binds one at run time. The image config's `Volumes` lists the destination, so
   Docker and Kubernetes still see a mount point; the name and its `FOR` travel in the
   normalized Agentfile.
9. **`--chown` (Q11)** is Docker's `--chown=<user>[:<group>]`, read in the domain it
   names. `--chown=<agent>` means that agent's own uid and gid, which the runtime
   assigns; numbers are allowed.
10. **`CONNECT` (Q13, Q14).** Its only option is `--target-kind` until relays are
    designed (12.14). Every agent a `CONNECT` names must be allowed by each network's
    `FOR`; otherwise the build fails, naming the agent, the network and both lines.
11. **Harnesses (Q18).** A harness runs as a domain of its own (§9.3). `ATTACH` lets it
    drive an agent through the in-VM server (send requests, read results) and nothing
    more: no read of the agent's directory, which would break §8's isolation. Naming an
    agent or harness never declared is a build error. The keyword stays `ATTACH`; the
    networks' wording becomes "join". Its artifact is `application/vnd.osi.harness.v1`,
    with the same tar+zstd content layers as an agent's. A `CONNECT` that needs both
    kinds is written as two `CONNECT`s. `SKILL`, `MCP` and `NETWORK` take
    `--target-kind` as `VOLUME` does.
12. **`SKILL --from=<agent>` (Q19.6)** is allowed: it copies, at build time, a skill
    the agent's config lists into the other's grants. That is a declaration in the
    Agentfile, not one agent reading another at run time.
13. **MCP instances (Q20)** live for one run of their caller and keep no state between
    runs: state kept longer would carry one run's data into the next.
14. **Relays and declassifiers (Q20)** wait for their own design; until then a path the
    closure finds (§9.5) is always a build error.
15. **Spawning (Q21).** `AGENT` and `HARNESS` take `--processes=<n|none>`. `none`
    installs the no-spawn filter of §9.9; without it a domain's `pids.max` is the
    microVM's own limit, until a measured default replaces it.
16. **Agentfiles from images (Q22)** are of one image, `FROM` it by digest, its history
    as comments. Compose files and rebuildable `RUN` history are later work.

## 11. Conformance: every directive, at build and at run

The Dockerfile reference (docs.docker.com/reference/dockerfile, read 2026-10-02) and
BuildKit's dockerfile/1.27.1, which `crates/dockerfile` is held to, list what an Agentfile
must do; the extensions (§4) are added to that list. Each item counts as done only when an
E2E test builds and runs it, against BuildKit and Docker where they define it. Status as
of 2026-10-02:

| Item | Build | Run |
|---|---|---|
| Parser directives `syntax`, `escape`, `check` | done (oracle) | n/a |
| `FROM` (image, scratch, stage, `--platform`, `AS`) | done for the host's platform; another platform's `RUN` needs an emulator in the guest | n/a |
| `ARG` (scopes, predefined proxy args, platform args, `BUILDKIT_*`, `SOURCE_DATE_EPOCH`) | planned (oracle); `BUILDKIT_*` and `SOURCE_DATE_EPOCH` effects to check | n/a |
| `ENV`, `LABEL`, `MAINTAINER`, `WORKDIR`, `USER`, `SHELL` | done (oracle, config) | `ENV`, `WORKDIR`, `USER` done; labels not shown by `inspect`/`images --filter` |
| `CMD`, `ENTRYPOINT` (both forms) | done | done |
| `COPY` (`--from`, `--chmod`, `--chown`, `--link`, `--parents`, `--exclude`, heredocs) | done (BuildKit's actions, byte for byte) | n/a |
| `ADD` of local files and archives (`--chmod`, `--chown`, `--link`, `--exclude`, `--unpack`) | done | n/a |
| `ADD` of URLs (`--checksum`, `--unpack`) and git (`--keep-git-dir`, `--checksum`) | **missing** | n/a |
| `RUN` (shell, exec, heredocs) | done, in one builder microVM per build (D34): every form the reference documents builds layer for layer as BuildKit builds it (`scripts/build/realworld/cases/run-forms`); a command that cannot start is said as BuildKit says runc's failure (its output, then exit code 1) | n/a |
| `RUN --mount` `bind`, `cache`, `tmpfs`, `secret`, `ssh` | `bind` (context, stage, image, `rw`), `tmpfs`, and `cache` within one build done (`run-forms`, E2E); the context's sources checksummed first as BuildKit's are; `secret` has no source until `--secret` is served; `ssh` **missing** | n/a |
| A step's paths resolved in its root (working directory, mount targets and sources, stubs), as runc and BuildKit resolve them, never through a planted symlink into the builder (`cases/in-root`, E2E, mutation-checked) | done | n/a |
| Defect, fixed (2026-10-02): `shards wait` gave 0, `ps` showed an ended run `Up` and `--rm` left its container, when a run ended before its container's record was written (the record is written on a thread of its own, which parallel E2E runs slow): the end was dropped (`end_container` saw no container in sight). A run's end now waits for its record, as a run that never starts did; `a_run_that_ends_before_its_record_is_written_keeps_its_end` holds the write to reproduce it | n/a | done |
| `RUN` on Windows hosts: a builder over WHP and a Windows transport | **missing** | n/a |
| `RUN`'s sandbox: moby's default seccomp profile, which BuildKit applies to every step (`Seccomp: 2`) | **missing** | n/a |
| A builder's memory plugged as a build needs it (virtio-mem), not paid at boot (PM M82) | **missing** | n/a |
| `RUN --network` `default`, `none`, `host`; `--security`; `--device` | `default` and `none` done (D31, E2E); `host` and `--security=insecure` only under `--allow`, `host` given the builder's own network; `--device` **missing** | n/a |
| `docker run`'s network | n/a | `--network` (and `--net`) `default`, `bridge` and `none` done (D31, E2E): the bridge's egress through the VM's network process, the run's own name at its address in `/etc/hosts` (on the loopback on `none`), the host's resolvers as dockerd gives them without IPv6; the long syntax and every check the CLI and dockerd make, with their words (Go's own answers, `tests/go-network.json`; dockerd's, measured). Deliberately unlike dockerd: `none` then `bridge` is refused as `bridge` then `none` is; several endpoints' errors come in the order given, not Go's map order; `/etc/resolv.conf` carries no comments naming an engine. **Missing**: `host`, `container:NAME` and user-defined networks (`docker network`), an endpoint's `mac-address`, `link-local-ip` and sysctls (refused up front), `-p`/`-P`, the DNS policy, the network process's confinement (seccomp, Landlock, App Sandbox), its costs (E1, E2) |
| `EXPOSE` | done (config) | **missing**: `run -P`, `-p` need D31 |
| `VOLUME` | done (config) | **missing**: anonymous volumes at run |
| `HEALTHCHECK` (`--interval`, `--timeout`, `--start-period`, `--start-interval`, `--retries`, `NONE`) | done (config) | **missing**: checks, `ps` status, `inspect` health |
| `STOPSIGNAL` | done (config) | **missing**: `stop` sends SIGTERM regardless |
| `ONBUILD` | triggers planned (oracle); run when their steps are | n/a |
| Build cache by step and whole image; `--cache-from/--cache-to`, `--no-cache` | **missing** | n/a |
| buildx flags not served (`--secret`, `--ssh`, `--build-context`, `--output`, `--push`, `--platform` lists, attestations) | **missing** | n/a |
| Extensions `EXPOSE … AS/FOR`, `AGENT`, `SKILL`, `MCP`, `VOLUME … FOR`, `NETWORK`, `CONNECT`, `HARNESS`, `ATTACH`, each expanding `ARG` and `ENV` as the instructions Docker expands them in do | **missing** | **missing** |
| Names (§4.10): `--target-kind`, one namespace of stages, agents and harnesses (Q19.2), the ambiguity error with both rewrites | **missing** | n/a |
| `COPY --from=<agent or harness>`, writes into a domain refused, `AGENT`/`HARNESS` layers as `COPY --link`, directives per stage (Q19) | **missing** | n/a |
| Agent and harness artifacts: OSI `artifactType`, config, content layers, index, pull and push (§8 Q1) | **missing** | **missing** |
| The normalized Agentfile as a layer of its own, its label and manifest annotations (§8) | **missing** | read by the runtime: **missing** |
| A BuildKit frontend, so `docker buildx build` builds an Agentfile (`# syntax=`, §8 Q16) | **missing** | n/a |
| OCI objects (§1): `push`, `tag`, `inspect`, `images`, `rmi`, `save`, `load`, OCI layouts and `docker save` tars taken in (§10) | `pull` done | **missing** |
| An Agentfile made from any image (§10) | **missing** | n/a |
| The in-VM runtime: many agents and harnesses per microVM, each a domain (§6, §9.3) | n/a | **missing** |
| The in-VM server: agents' encrypted, deny-by-default communication, the code-mode MCP server they discover (§5) | n/a | **missing** |
| Build-time isolation checks (§9.2) and transitive reach, relays, declassifiers (§9.5, §9.8) | **missing** | n/a |
| Run-time confinement (§9.3, §9.7–9.9): namespaces, IDs, cgroups and `pids.max`, Landlock, seccomp, `io_uring` off, vsock closed to workloads, process events | n/a | **missing** |
| Labels on data through the in-VM server and MCP results; per-caller MCP instances (§9.5, §9.6) | n/a | **missing** |
| The escape tests of §9.10, each mutation-checked | **missing** | **missing** |

The order follows what depends on what: `RUN` first, since nearly every real file needs
it (808 of 822 official Dockerfiles), then guest networking (D31), which `RUN`'s default
network, `EXPOSE`'s publishing and `NETWORK`/`CONNECT` all need; then sources, the cache,
the run-time directives, the OCI objects, and then the agents' layer: artifacts, the
extensions, the in-VM runtime and server, and isolation with its tests. Where §7 still
holds an open question, the item is built on the recommended answer recorded there, or on
a decision recorded beside it with its reason, for the review to confirm.
