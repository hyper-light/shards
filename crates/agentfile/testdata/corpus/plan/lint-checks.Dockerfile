# check=experimental=InvalidDefinitionDescription
ARG BASE=
ARG password=secret
FROM ${BASE:-alpine} as Builder
FROM $NOPE AS nope
FROM alpine AS scratch
FROM alpine AS dup
FROM alpine AS dup
from alpine
Run echo mixed
ENV API_KEY=x PUBLIC_KEY=y
WORKDIR relative
WORKDIR /abs
EXPOSE 80/TCP
