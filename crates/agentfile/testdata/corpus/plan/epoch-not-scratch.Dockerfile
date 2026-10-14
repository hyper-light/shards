FROM alpine AS src
ADD https://example.com/a.tar /

FROM alpine
RUN true
