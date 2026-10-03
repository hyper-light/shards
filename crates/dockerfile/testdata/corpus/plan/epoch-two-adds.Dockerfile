FROM scratch AS src
ADD https://example.com/a.tar /
ADD https://example.com/b.tar /

FROM alpine
RUN true
