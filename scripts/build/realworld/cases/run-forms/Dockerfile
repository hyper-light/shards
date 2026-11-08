# RUN in each form the Dockerfile reference documents (docs.docker.com/reference/dockerfile,
# "Here-documents" and "RUN --mount"), over a real base, for compare.py to hold shards'
# build to BuildKit's.
FROM alpine:3.22 AS other
RUN echo from another stage > /other

FROM alpine:3.22 AS base
RUN <<EOF
set -e
mkdir -p /opt/here
echo "one $((1+1))" > /opt/here/a
EOF
RUN <<EOT sh
echo second > /opt/here/b
EOT
RUN <<FILE1 cat > /opt/here/c && <<FILE2 cat > /opt/here/d
I am
first
FILE1
I am
second
FILE2
COPY <<-EOT /opt/here/e
	tabs stripped
	EOT
COPY <<EOF /opt/here/f
home is $HOME
EOF
RUN ["sh", "-c", "echo exec form > /opt/exec"]
RUN --mount=type=tmpfs,target=/scratch echo t > /scratch/t && ls /scratch > /opt/tmpfs-listing
RUN --mount=type=cache,target=/cache,id=c1 echo kept > /cache/k
RUN --mount=type=cache,target=/cache,id=c1 cat /cache/k > /opt/from-cache
RUN --mount=type=bind,source=ctxfile,target=/ctx cat /ctx > /opt/from-bind
RUN --mount=type=bind,from=other,source=/other,target=/rel cp /rel /opt/rel
RUN --mount=type=bind,target=/ctxdir,rw touch /ctxdir/written && ls /ctxdir > /opt/ctx-listing
RUN --mount=type=bind,target=/ctxdir test ! -e /ctxdir/written && echo discarded > /opt/rw-discarded
CMD ["sh", "-c", "cd /opt && for f in here/* exec tmpfs-listing from-cache from-bind rel ctx-listing rw-discarded; do echo \"== $f\"; cat $f; done; test ! -e /scratch/t && echo tmpfs-gone"]
