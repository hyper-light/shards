FROM alpine:3.20
RUN --mount=type=cache,target=/var/cache/x,id=shards-d113-mounts echo cached > /var/cache/x/f && cp /var/cache/x/f /cached
RUN --mount=type=tmpfs,target=/scratch ls /scratch > /tmp-listing
RUN --mount=type=bind,source=ctx.txt,target=/ctx.txt cp /ctx.txt /copied
RUN <<EOF
echo first > /here1
echo second > /here2
EOF
COPY <<EOF /etc/greeting
hello from a heredoc
EOF
