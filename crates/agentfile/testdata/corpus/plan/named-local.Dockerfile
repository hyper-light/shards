FROM alpine
COPY --from=src /x /x
COPY --from=src /y /y
