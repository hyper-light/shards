# The base this builds on.
ARG BASE=alpine:3.20

# Builds the tool.
FROM ${BASE} AS build
# The version to build.
ARG VERSION=1.0
RUN --mount=type=secret,id=token echo "$VERSION" > /v

FROM build AS final
RUN --mount=type=ssh true
