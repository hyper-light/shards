FROM alpine AS a
RUN true
FROM a
COPY --from=withconfig / /b
