# The agent artifact, and Agentfiles as OCI objects

Research note, 2026-10-01, for AGENTFILE_ARCH.md Q1 (the agent is an OCI artifact of its
own type) and Q16 (an Agentfile, with a BuildKit frontend). Sources pinned: image-spec
`ca68a05`, distribution-spec `9727462`, BuildKit v0.33.1 (`8c91502c`), firecracker-
containerd `be68640`, distribution/distribution `1d71a5c7`, `../firecracker`, `../go-microvm`
(`7e148d8`). Proposals are marked; nothing here is decided.

## 1. OCI 1.1 artifacts

- **`artifactType`** must be set when the config is the empty type, follows RFC 6838 §4.2
  naming, needs no IANA registration, and an unknown one "MUST NOT" be an error
  [image-spec manifest.md:29-34]; an index may carry one too [image-index.md:25].
- **A config of unknown media type** is opaque bytes, stored without failing
  [manifest.md:36-56]. The empty descriptor is `application/vnd.oci.empty.v1+json`, `{}`
  [manifest.md:160-169].
- **Guidelines for artifact usage** [manifest.md:176-264]: no blobs (empty config, one
  empty layer); blobs without a config (empty config, content as layers); blobs and a
  config (its own config type and layers). Readers fall back to `config.mediaType` when
  `artifactType` is absent.
- **`subject` and referrers.** Registries accept a `subject` not yet present
  [distribution-spec spec.md:221], answer `OCI-Subject` [:548], list referrers at
  `GET /v2/<name>/referrers/<digest>` filtered by `?artifactType=` [:621-634, 679-681], and
  a 404 there sends clients to the referrers tag schema [:712-714, 784-791].
- **Annotations** use reverse-domain keys; `org.opencontainers.image.*` is the spec's
  [annotations.md:12-14].
- **Precedents.** Helm: config `application/vnd.cncf.helm.config.v1+json`, layer
  `application/vnd.cncf.helm.chart.content.v1.tar+gzip` [helm pkg/registry/constants.go].
  Wasm: config `application/vnd.wasm.config.v0+json` with `architecture`, `os`,
  `layerDigests`, `component` [tag-runtime.cncf.io wasm-oci-artifact]. Notation:
  `artifactType application/vnd.cncf.notary.signature`, empty config, `subject` the signed
  manifest [notaryproject specs/signature-specification.md]. Flux:
  `application/vnd.cncf.flux.config.v1+json` [fluxcd/pkg oci/globals.go:27-30].
  BuildKit's attestations: in-toto layers, and with `oci-artifact=true` an
  `artifactType application/vnd.docker.attestation.manifest.v1+json` manifest with a
  `subject` [buildkit exporter/containerimage/writer.go:51; docs/attestations/
  attestation-storage.md].
- **Registries.** ECR supports OCI 1.1 referrers (AWS, 2024-06); Docker Hub documents
  artifacts by config media type, not referrers [docs.docker.com/docker-hub/repos/manage/
  hub-images/oci-artifacts/]; GHCR documents neither; distribution/distribution has no
  referrers endpoint or `subject` handling, so clients use the tag fallback. Docker Hub's
  and GHCR's referrers are unverified.

## 2. What Firecracker and go-microvm do with OCI

- **Firecracker has none.** Its docs point to firecracker-containerd [../firecracker
  FAQ.md:47-48]; kernels and root filesystems are raw files [docs/getting-started.md:98-137].
- **firecracker-containerd** runs ordinary container images inside a VM: containerd pulls
  them, a block-device snapshotter (naive or devmapper) makes each snapshot a drive,
  because Firecracker shares no filesystem [docs/snapshotter.md:3-7], hot-patched in as a
  stub drive [runtime/drive_handler.go:45-53], and an agent in the VM runs runc
  [docs/architecture.md:37-41]. The VM's own kernel and root are host paths in its config
  [docs/getting-started.md:218-227]. It defines no artifact types.
- **go-microvm** flattens an image's layers into a root directory and writes
  `/.krun_config.json` from its config [docs/ARCHITECTURE.md:203-350], and publishes its
  runtime and firmware as ORAS artifacts, `application/vnd.stacklok.go-microvm.runtime`, one
  gzip layer, the platform in the tag [.github/workflows/release.yaml:171-181]: unversioned,
  and not an index, which shards should not copy.

## 3. BuildKit frontends (v0.33.1)

- `# syntax=` or `BUILDKIT_SYNTAX` sends the build to `gateway.v0` with the frontend image
  [frontend/dockerfile/builder/build.go:34, 55-68, 227-258]; a frontend is an image whose
  main runs `grpcclient.RunFromEnvironment` [frontend/dockerfile/cmd/dockerfile-frontend/
  main.go:30]. The default file is `Dockerfile` [frontend/dockerui/context.go:31, 37], so
  an Agentfile is built with `-f Agentfile`.
- `parser.Parse` keeps unknown instructions [parser/parser.go:232-235]; only
  `instructions.ParseInstruction` refuses them [instructions/parse.go:145]; `Dockerfile2LLB`
  takes bytes [dockerfile2llb/convert.go:84]. A frontend can take the agent directives out
  of the AST and hand the rest on, its own LLB merged in.
- **It can return:** each platform's image config [frontend/dockerui/build.go:76, 85];
  manifest and index annotations, which the client's own exporter options override
  [exporter/containerimage/export.go:224-235]; attestations [solver/result/result.go:45].
- **It cannot:** write a manifest of its own `artifactType`, change the layer format, or
  change what happens at run time. BuildKit's image walker refuses unknown config types
  [util/imageutil/config.go:191-196]; `llb.ImageBlob` fetches a blob by digest
  [client/llb/source.go:116], and whether an artifact's manifest resolves through it is
  unverified.
- The classic Docker image store drops indexes and attestations; registries and the
  containerd store keep them [docs.docker.com/build/metadata/attestations/].

## 4. Proposals, for the review

**The agent artifact** (proposal), case 3 of the guidelines, as Helm and Wasm do:

- `artifactType` `application/vnd.shards.agent.v1`; config
  `application/vnd.shards.agent.config.v1+json`; content layers
  `application/vnd.shards.agent.content.v1.tar+zstd` (and `+gzip`), a tar rooted at the
  agent's directory, without whiteouts.
- The config: `schemaVersion`, `name`, `version`, how it runs (entrypoint, arguments,
  environment, relative to its directory), `os` and `architecture` (or any), the skills
  and MCP servers it brings, and the capabilities it asks for, which an Agentfile grants or
  refuses. What an agent is, and how shards runs it, is Q1's remaining part.
- Several platforms: an index with `artifactType`, one manifest per platform.
- `.v1` in every type and `schemaVersion` in the config; readers refuse a major they do not
  know.
- `AGENT ... FROM`: resolve the tag to a digest, check `artifactType` and platform, verify
  every blob, record the digest in the image and the build cache; optionally require a
  signature found among its referrers. The agent has no `subject`; signatures and SBOMs
  refer to it.

**Directives in a built image** (proposal), by how far each survives:

1. Canonical: the normalized Agentfile spec as a file in its own layer, which survives
   every store and copy, and is what shards' runtime reads.
2. Config labels: its digest and a summary, which survive the classic store and inspect.
3. Manifest annotations: discovery in registries.
4. Attestations or referrers: provenance and signed policy only, never anything the runtime
   needs.

Other engines keep 1 and 2 and enforce neither (D32).

**The frontend** (proposal): an image named by `# syntax=`, built with `-f Agentfile`. It
parses with BuildKit's parser, rewrites the agent directives (agent lines commented out to
keep line numbers; `SKILL` and `MCP` sources as `ADD` and `COPY`), adds its own LLB for
agent content and skill checks, and returns the image config with the spec layer, labels,
annotations and an optional attestation. Its limits: no agent artifacts and no pushes of
them (that is `shards build` and `shards push`), no EROFS layers, nothing enforced at run
time, `CapSourceImageBlob` for direct blob fetches, and annotations lost in the classic
store.
