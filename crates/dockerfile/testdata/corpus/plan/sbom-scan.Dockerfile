FROM alpine AS deps
ARG BUILDKIT_SBOM_SCAN_STAGE=true
RUN echo deps
FROM alpine AS other
RUN echo other
FROM alpine
ARG BUILDKIT_SBOM_SCAN_CONTEXT=true
ARG BUILDKIT_SBOM_SCAN_STAGE=other
COPY --from=deps / /d
COPY --from=other / /o
COPY a /a
