FROM alpine
RUN echo hi
FROM alpine:latest AS two
RUN echo two
