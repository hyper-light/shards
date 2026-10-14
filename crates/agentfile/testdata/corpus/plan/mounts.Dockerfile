FROM alpine
RUN --mount=type=cache,target=/c,id=cid,sharing=locked --mount=type=tmpfs,target=/t,size=64m --mount=type=bind,source=.,target=/src --mount=type=secret,id=s --network=none --security=sandbox make
