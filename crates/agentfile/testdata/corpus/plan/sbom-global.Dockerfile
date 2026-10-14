ARG BUILDKIT_SBOM_SCAN_STAGE=lib
FROM alpine AS lib
RUN echo lib
FROM alpine AS skipped
RUN echo skipped
FROM alpine
ARG BUILDKIT_SBOM_SCAN_STAGE
COPY --from=lib / /l
