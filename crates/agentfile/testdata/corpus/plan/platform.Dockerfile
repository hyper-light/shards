FROM --platform=$BUILDPLATFORM alpine AS b
ARG TARGETARCH
RUN echo $TARGETARCH
FROM alpine
COPY --from=b /x /x
