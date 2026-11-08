# base is shared by all
FROM --platform=$BUILDPLATFORM alpine AS base
FROM base AS dev
# release is what <ships> & runs
FROM base AS release
FROM release
