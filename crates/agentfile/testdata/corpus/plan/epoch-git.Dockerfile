FROM scratch AS src
ADD https://github.com/moby/buildkit.git#v0.20.0 /

FROM alpine
RUN true
