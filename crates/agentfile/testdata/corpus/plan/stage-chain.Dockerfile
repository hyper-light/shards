FROM alpine AS base
ENV A=1
WORKDIR /app
USER 1000
FROM base AS mid
ENV B=2
RUN env
FROM mid AS unused
RUN echo never
FROM mid
COPY --from=base /app /copy
RUN echo $A $B
