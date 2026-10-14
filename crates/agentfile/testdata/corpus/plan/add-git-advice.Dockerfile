FROM alpine
ADD https://github.com/moby/buildkit.git#v0.28.1 /src
ADD --keep-git-dir https://github.com/moby/buildkit.git /k
