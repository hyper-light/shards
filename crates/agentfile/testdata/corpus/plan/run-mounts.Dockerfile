FROM alpine AS src
RUN mkdir -p /build/out
FROM alpine
RUN --mount=type=bind,from=src,source=/build,target=/b --mount=type=bind,rw,target=/ctx make
RUN --mount=type=cache,target=rel/cache,uid=1000,gid=1000,mode=0700 --mount=type=cache,target=/c2,sharing=private,from=src,source=/build go build
RUN --mount=type=secret,id=tok,env=TOKEN --mount=type=secret,id=f,target=/s/f,uid=5,mode=0440,required echo $TOKEN
RUN --mount=type=secret,source=npmrc --mount=type=secret,target=/only/target cat /run/secrets/npmrc
RUN --mount=type=ssh --mount=type=ssh,id=deploy,target=/agent.sock,mode=0666,required git clone
RUN --mount=type=tmpfs,target=/tmp,size=1g --mount=type=tmpfs,target=/t2 ls
RUN --network=host --security=insecure ping x
RUN --device=nvidia.com/gpu=all --device=name=vendor.com/x=1,required=true nvidia-smi
