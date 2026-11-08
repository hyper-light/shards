# syntax=127.0.0.1:15113/shards-d113-spy:1
FROM alpine:3.20
COPY a.txt /a
RUN echo hi > /b
