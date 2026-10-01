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
AGENT <name> FROM <registry>[:<tag>] [TO <path>]
```

- `AS` is optional.
- `<name>` is required. It names the agent, as a build stage's alias names an image.
  `AGENT main FROM some.registry.com/claude-opus-5-5` declares an agent called `main`.
- `<registry>` must name, in a valid OCI-compliant registry, a tarball that contains the
  agent. How that tarball is laid out is still to be specified.
- When a build meets an `AGENT` directive, it downloads the tarball and decompresses it to:
  - `TO <path>`, if given;
  - otherwise `/agents/<name>[_<tag>]`.
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
MCP <name>[:<tag>] FROM <path | uri | url | git | oci_artifact>[:<port>] [FOR <agent_name>]
```

`MCP` declares an MCP server, local or remote, that the agents connect to.

**A URL or URI** is a remote server:
- Without a port, the port is 8000. HTTPS is respected.
- The agents' workloads/workspaces are opened to receive traffic on that port, and so is
  the microVM: exposed for ingress and egress.

**A path, git URL or OCI artifact** is a server spoken to over stdio. Agents are opened to
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
  - `--type=<harness|agent|default>` (added 2026-10-01): whether the names after `FOR`
    are harnesses or agents (§4.10).

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
- `--type=<harness|agent>` (added 2026-10-01) says whether the names are harnesses or
  agents (§4.10).
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
- **`VOLUME` and `CONNECT`** name harnesses as they name agents, told apart by `--type`
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
never resolve a name to the wrong kind. So where a directive may name either (`VOLUME`,
`CONNECT`), a name that belongs to both an agent and a harness is a build error unless
`--type` says which is meant; it never defaults to one kind. A name that belongs to one
kind alone resolves to that kind.

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
  There is no `shards build` yet.
- **Networking.** No network device exists. A microVM is reached only over vsock, so it is
  airgapped today, the default this spec keeps. `EXPOSE`, `NETWORK`, `CONNECT` and remote
  `MCP` servers will reach the network through a network process of the microVM's own, never
  through the VM process, which stays confined without TCP; that process holds the policy
  they declare (architecture.md D31).
- **Agents' workspaces.** The containerd-like runtime inside each microVM runs many agents
  per VM, each isolated as a container would be, without being containers. It is where the
  workloads/workspaces of §4.2 live. Mounts go to one workload, some or all, attached once
  (architecture.md D17). That is how `VOLUME ... FOR`, `SKILL ... FOR` and `MCP ... FOR`
  would reach their agents, and `HARNESS ... FOR` its harnesses.
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
    - What does `VOLUME --type=default` mean: Docker's own `VOLUME`, a mount point with no
      agent or harness scope? May `FOR` follow it?
    - Do skills and MCP servers reach harnesses too, and do `SKILL`, `MCP` and `NETWORK`
      take `--type`?
    - Does unpacking to `/harness/<name>` omit the tag, where agents go to
      `/agents/<name>[_<tag>]`?

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
