FROM alpine:3.20 AS deps
ARG BUILDKIT_SBOM_SCAN_STAGE=true
COPY note /deps-note

FROM alpine:3.20
ARG BUILDKIT_SBOM_SCAN_CONTEXT=true
COPY --from=deps /deps-note /note
