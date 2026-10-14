FROM alpine AS first
RUN echo zero
FROM alpine
RUN echo one
RUN echo two
