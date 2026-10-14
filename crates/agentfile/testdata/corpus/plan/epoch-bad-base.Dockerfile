FROM ${X:?missing} AS src
ADD https://example.com/a.tar /

FROM alpine
RUN true
