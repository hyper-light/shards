FROM scratch AS kelvins
ADD https://example.com/ /

FROM alpine
RUN true
