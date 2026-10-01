FROM --platform=linux/arm64 armimg AS arm
RUN uname -m
FROM --platform=$TARGETPLATFORM alpine AS redundant
FROM --platform=linux/amd64 alpine AS const
FROM --platform=$BUILDPLATFORM alpine
COPY --from=arm /x /x
