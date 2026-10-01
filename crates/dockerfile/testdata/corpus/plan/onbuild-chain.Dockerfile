FROM alpine AS base
ONBUILD RUN echo from-base
ONBUILD COPY --from=base /etc/hosts /hosts
FROM base AS child
RUN echo child
FROM child
RUN echo grandchild
