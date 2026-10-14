# syntax=docker/dockerfile:1
# escape=`
# check=skip=all
FROM alpine
RUN echo a `
  b
