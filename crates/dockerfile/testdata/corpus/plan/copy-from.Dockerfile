FROM alpine AS one
RUN mkdir /out
FROM scratch
COPY --from=0 /out /out0
COPY --from=one /out/./a* /out1/
COPY --from=alpine /etc/passwd /passwd
COPY --from=registry.example.com:5000/team/app:1.0 /app /app
