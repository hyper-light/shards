FROM scratch AS src
ADD https://example.com/a.tar /
run true

FROM alpine
RUN true
