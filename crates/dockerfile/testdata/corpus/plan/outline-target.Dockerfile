# OS names the distribution
ARG OS=alpine
# BASE is the image every stage builds on
ARG BASE=${OS}
ARG UNUSED=nothing

# build compiles the program
FROM ${BASE} AS build
# VERSION is stamped into the binary
ARG VERSION=dev
RUN --mount=type=secret,id=token,required=true \
    --mount=type=ssh echo $VERSION

FROM build AS test
ARG TEST_FLAGS
RUN --mount=type=secret,id=npmrc echo $TEST_FLAGS

# final is what ships
FROM alpine AS final
ARG OS
COPY --from=test /x /x
RUN --mount=type=ssh,id=gh,required=true --mount=type=secret,target=/run/key true
