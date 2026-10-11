FROM scratch AS base
COPY a /a
ENV FROM_BASE=1
WORKDIR /w

FROM base AS app
COPY b /b
