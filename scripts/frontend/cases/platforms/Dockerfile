FROM --platform=$BUILDPLATFORM alpine:3.20 AS b
ARG TARGETPLATFORM
RUN echo "$TARGETPLATFORM" > /p

FROM scratch
COPY --from=b /p /p
