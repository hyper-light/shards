FROM scratch AS src
ADD --checksum=sha256:abc https://example.com/a.tar /

FROM alpine
RUN true
