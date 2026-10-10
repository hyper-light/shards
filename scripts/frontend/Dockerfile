# shards' BuildKit frontend (D113): FROM scratch, the static musl `shards` alone, run as
# `shards frontend`, as BuildKit runs a frontend a `# syntax=` line names
# (frontend/gateway/gateway.go: the entrypoint, no network where the label says so, the
# frontend capabilities the label lists checked against what the client asks). Laid out
# as BuildKit's own docker/dockerfile is (its history: one binary COPYed onto scratch,
# its labels, its entrypoint). scripts/frontend/image builds it; the caps label is
# frontend::CAPS, which a test of the crate holds it to.
FROM scratch
LABEL moby.buildkit.frontend.network.none="true"
LABEL moby.buildkit.frontend.caps="moby.buildkit.frontend.subrequests,moby.buildkit.frontend.contexts,moby.buildkit.frontend.gitquerystring,moby.buildkit.frontend.contexts.zstd"
COPY shards /shards
ENTRYPOINT ["/shards", "frontend"]
