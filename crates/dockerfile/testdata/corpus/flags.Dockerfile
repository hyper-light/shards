FROM alpine
COPY --from=a --chown="x y" '--chmod=0755' src dst
COPY -- --notaflag x
RUN --mount=type=cache,target=/x --network=none echo hi
ADD --checksum=sha256:00 --  http://x y
COPY --a\ b c d
COPY --x="q\"r" s t
