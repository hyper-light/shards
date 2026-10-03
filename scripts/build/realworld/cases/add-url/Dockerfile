# ADD of URLs, as BuildKit adds them: a file named for its URL's path, mode 0600 unless
# --chmod says, its mtime the response's Last-Modified or the epoch; a pinned checksum; an
# archive left whole unless --unpack; a redirect followed.
FROM alpine:3.22
ADD https://raw.githubusercontent.com/moby/buildkit/dockerfile/1.27.1/README.md /docs/
ADD --chmod=644 https://raw.githubusercontent.com/moby/buildkit/dockerfile/1.27.1/LICENSE /license
ADD --checksum=sha256:c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 https://raw.githubusercontent.com/moby/buildkit/dockerfile/1.27.1/LICENSE /checked/
ADD https://github.com/opencontainers/runtime-spec/archive/refs/tags/v1.2.0.tar.gz /archive.tar.gz
ADD --unpack=true https://github.com/opencontainers/runtime-spec/archive/refs/tags/v1.2.0.tar.gz /unpacked/
