FROM alpine AS base
RUN echo one
RUN --network=default echo two

FROM base
RUN --mount=type=tmpfs,target=/scratch echo three
