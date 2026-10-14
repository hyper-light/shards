ARG SOURCE_DATE_EPOCH
FROM ${SOURCE_DATE_EPOCH:+not}scratch AS src
ADD https://example.com/${SOURCE_DATE_EPOCH}a.tar /

FROM alpine
RUN true
