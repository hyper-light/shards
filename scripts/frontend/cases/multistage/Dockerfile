FROM alpine:3.20 AS build
RUN mkdir -p /out && echo built > /out/artifact && printf 'x\n' > /out/x

FROM busybox:1.36 AS tools

FROM scratch
COPY --from=build /out/ /
COPY --link --from=tools /bin/busybox /bin/busybox
ENV PATH=/bin
CMD ["busybox", "cat", "/artifact"]
