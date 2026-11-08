FROM alpine AS base
ONBUILD RUN --mount=type=bind,from=nowhere,target=/x ls
FROM base
