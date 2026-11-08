FROM alpine
ADD https://example.com/files/app.tar.gz /opt/
ADD --checksum=sha256:24454f830cdb571e2c4ad15481119c43b3cafd48dd869a9b2945d1036d1dc68d https://example.com/x /x
ADD --chmod=644 --chown=1:1 http://example.com/ /root-file
ADD --unpack=true https://example.com/a.tar /a
ADD https://github.com/moby/buildkit.git#v0.12.0:docs /docs
ADD --keep-git-dir=true https://github.com/moby/buildkit.git?ref=main&subdir=frontend /fe
ADD https://example.com/r.git?branch=dev&submodules=false&fetch-by-commit /r
ADD git@nonexistent.invalid:org/repo.git#main /ssh
ADD --checksum=0123456789abcdef https://example.com/r.git /sum
ADD git://example.com/r.git /plain
ADD github.com/moby/buildkit /looks-local
