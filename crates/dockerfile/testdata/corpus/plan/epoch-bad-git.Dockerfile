FROM scratch AS src
ADD git@github.com:moby/buildkit.git?branch=b&tag=t /

FROM alpine
RUN true
