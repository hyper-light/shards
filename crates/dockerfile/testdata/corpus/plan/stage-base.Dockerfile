FROM alpine AS base
ENV B=1
FROM base
RUN echo $B
