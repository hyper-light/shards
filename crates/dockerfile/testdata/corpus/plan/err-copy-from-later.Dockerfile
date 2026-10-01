FROM alpine AS a
COPY --from=b /x /y
FROM alpine AS b
