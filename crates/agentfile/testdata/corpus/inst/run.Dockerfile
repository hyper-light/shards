FROM a
RUN --mount=type=cache,target=/c,id=x,sharing=locked,mode=0755,uid=1,gid=2 --network=none --security=insecure echo hi
RUN --mount=from=st,source=/s,target=/t,ro --mount=type=secret,id=s,required --mount=type=tmpfs,size=1g,target=/tmp --mount=type=ssh true
RUN --device=nvidia.com/gpu=all,required --device=name=x,required=false ["a", "b"]
RUN <<EOF
set -e
echo $A
EOF
RUN --network=host --mount="type=bind,source=a,target=b" x
RUN --mount=type=secret,target=/s,env=SECRET true
