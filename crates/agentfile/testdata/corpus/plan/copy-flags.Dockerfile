FROM alpine
WORKDIR /w
COPY --chown=app:staff --chmod=u+rwx,g+rx a rel/
COPY --chown=1000 b ./
COPY --chown=root:0 c .
COPY --exclude=*.tmp --exclude=cache d /d
COPY --parents x/y/./z/*.go /src/
COPY --parents p/q.txt /p/
COPY --chmod=755 e /e
ADD --chown=1:1 f.tar.gz /f/
ADD --unpack=false g.tar /g
ADD --link h /h
COPY --link --chown=2 i /i
COPY --link --chmod=600 j /j
